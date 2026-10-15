// The app page: issue a dollar, mint & redeem, and harvest yield, against
// the program on whatever cluster VITE_RPC_URL points at.

import "./polyfill";
import "./styles.css";

import { Connection, PublicKey } from "@solana/web3.js";
import {
  DEFAULT_PROGRAM_ID, ROUTE_DOLLAR, ROUTE_USDC, Tender, formatUsd, friendlyError, harvestable, parseUsd,
  reserveValue, validSymbol, type IssuerState, type ReserveState, type Route,
} from "./chain/tender";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import { connectWallet, disconnectWallet, signAndSend, walletKey } from "./chain/wallet";

const CLUSTER = (import.meta.env.VITE_CLUSTER || "mainnet-beta").trim();
// Solana's own mainnet RPC refuses requests from websites; PublicNode allows
// them but won't list every dollar (getProgramAccounts).
const DEFAULT_RPC = CLUSTER === "devnet" ? "https://api.devnet.solana.com" : "https://solana-rpc.publicnode.com";
/** Problems with the site's settings, shown in the notice instead of crashing. */
const configProblems: string[] = [];

/**
 * VITE_RPC_URL may be a full URL or just a Helius API key. Anything else is
 * ignored (with a notice) so a bad setting never stops the page from working.
 */
function resolveRpc(raw: string | undefined): string {
  const v = (raw || "").trim().replace(/^["']|["']$/g, "");
  if (!v) return DEFAULT_RPC;
  if (/^https?:\/\//i.test(v)) return v;
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v)) {
    return `https://${CLUSTER === "devnet" ? "devnet" : "mainnet"}.helius-rpc.com/?api-key=${v}`;
  }
  configProblems.push("<code>VITE_RPC_URL</code> isn't a URL or a Helius key, so the site is using a public connection instead.");
  return DEFAULT_RPC;
}
function resolveProgram(raw: string | undefined): PublicKey {
  const v = (raw || "").trim();
  if (!v) return DEFAULT_PROGRAM_ID;
  try { return new PublicKey(v); } catch {
    configProblems.push("<code>VITE_PROGRAM_ID</code> isn't a valid address, so the site is using the default program.");
    return DEFAULT_PROGRAM_ID;
  }
}

const RPC = resolveRpc(import.meta.env.VITE_RPC_URL);
const TBILL_SYMBOL = import.meta.env.VITE_TBILL_SYMBOL || "USDY";
const PROGRAM = resolveProgram(import.meta.env.VITE_PROGRAM_ID);

const conn = new Connection(RPC, "confirmed");
const t = new Tender(conn, PROGRAM);

const $ = <T extends HTMLElement = HTMLElement>(s: string) => document.querySelector(s) as T;
const short = (k: PublicKey | string) => { const s = k.toString(); return `${s.slice(0, 4)}…${s.slice(-4)}`; };
const explorer = (sig: string) => {
  const q = CLUSTER === "mainnet-beta" ? "" : CLUSTER === "custom" ? `?cluster=custom&customUrl=${encodeURIComponent(RPC)}` : `?cluster=${CLUSTER}`;
  return `https://explorer.solana.com/tx/${sig}${q}`;
};

let reserve: ReserveState | null = null;
let vaults = { usdc: 0n, tbill: 0n };
let issuers: IssuerState[] = [];
let editing: IssuerState | null = null;

// ---------------------------------------------------------------- ui helpers

let toastTimer = 0;
const hideToast = () => ($("#toast").className = "toast");
function toast(html: string, err = false) {
  const el = $("#toast");
  el.innerHTML = html;
  el.className = "toast show" + (err ? " err" : "");
  clearTimeout(toastTimer);
  // Long enough to reach the transaction link; hovering keeps it open.
  toastTimer = window.setTimeout(hideToast, 10_000);
}
$("#toast").addEventListener("mouseenter", () => clearTimeout(toastTimer));
$("#toast").addEventListener("mouseleave", () => { toastTimer = window.setTimeout(hideToast, 3_000); });

async function run(btn: HTMLButtonElement, label: string, fn: () => Promise<string>) {
  const was = btn.textContent;
  btn.disabled = true;
  btn.textContent = label;
  try {
    const sig = await fn();
    toast(`Done. <a href="${explorer(sig)}" target="_blank" rel="noopener">View transaction</a>`);
    await refresh();
  } catch (e) {
    console.error(e);
    toast(friendlyError(e), true);
  } finally {
    btn.disabled = false;
    btn.textContent = was;
  }
}

function needWallet(): PublicKey {
  const k = walletKey();
  if (!k) throw new Error("Connect a wallet first.");
  return k;
}
function needReserve(): ReserveState {
  if (!reserve) throw new Error("No Tender reserve on this cluster yet.");
  return reserve;
}

// ---------------------------------------------------------------- tabs

// Clean addresses: /issue[/SYMBOL], /mint[/SYMBOL], /dollars, /admin.
const TABS = ["issue", "mint", "dollars", "admin"] as const;
type Tab = (typeof TABS)[number];

function route(): { tab: Tab; symbol: string } {
  const [, first = "", second = ""] = location.pathname.split("/");
  const tab = (TABS as readonly string[]).includes(first) ? (first as Tab) : "issue";
  return { tab, symbol: decodeURIComponent(second).toUpperCase().slice(0, 10) };
}

/** Old links (/app.html#convert?dollar=X) become clean ones (/mint/X). */
function upgradeOldLink() {
  if (!location.hash && TABS.some((t) => location.pathname.startsWith(`/${t}`))) return;
  const [tab = "", query = ""] = location.hash.slice(1).split("?");
  const q = new URLSearchParams(query);
  const name = tab === "convert" ? "mint" : (TABS as readonly string[]).includes(tab) ? tab : "issue";
  const sym = q.get("dollar") || q.get("symbol") || "";
  history.replaceState(null, "", `/${name}${sym ? `/${encodeURIComponent(sym.toUpperCase())}` : ""}`);
}

function go(path: string) {
  if (path !== location.pathname) history.pushState(null, "", path);
  showTab();
}

function showTab() {
  const { tab, symbol } = route();
  const pane = tab === "mint" ? "convert" : tab;
  document.querySelectorAll(".pane").forEach((p) => p.classList.toggle("on", p.id === `pane-${pane}`));
  document.querySelectorAll("[data-tab]").forEach((a) => a.classList.toggle(a.closest(".tabs") ? "on" : "active", (a as HTMLElement).dataset.tab === tab));
  document.title = `Tender · ${{ issue: "Issue", mint: "Mint & redeem", dollars: "Dollars", admin: "Admin" }[tab]}`;
  if (symbol && tab === "issue" && !editing) {
    $<HTMLInputElement>("#f-symbol").value = symbol;
    $("#coin-sym").textContent = symbol;
  }
  if (symbol && tab === "mint" && issuers.some((i) => i.symbol === symbol)) {
    $<HTMLSelectElement>("#c-issuer").value = symbol;
    refreshBalances();
  }
}
window.addEventListener("popstate", showTab);

// Tab links switch in place instead of reloading the page.
document.addEventListener("click", (e) => {
  const a = (e.target as Element).closest?.("a");
  if (!a || a.target || a.origin !== location.origin || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
  const first = a.pathname.split("/")[1];
  if (!(TABS as readonly string[]).includes(first)) return;
  e.preventDefault();
  go(a.pathname);
});

// ---------------------------------------------------------------- wallet

async function onWallet() {
  const k = walletKey();
  const b = $<HTMLButtonElement>("#connect");
  b.textContent = k ? short(k) : "Connect wallet";
  b.classList.toggle("ghost", !!k);
  b.classList.add("wallet-pill");
  if (k && !document.querySelector("#routes .route")) addRoute({ wallet: k, bps: 10_000, kind: ROUTE_USDC });
  await refreshBalances();
  renderDollars();
  renderAdmin();
}
$("#connect").addEventListener("click", async () => {
  try {
    if (walletKey()) await disconnectWallet();
    else await connectWallet();
  } catch (e) { toast(friendlyError(e), true); }
  onWallet();
});

// ---------------------------------------------------------------- data

async function refresh() {
  try {
    reserve = await t.fetchReserve();
  } catch (e) {
    showNotice(window.top !== window
      ? `This preview can't reach Solana: pages shown inside Claude are blocked from outside servers and wallets. Open the hosted site to issue, mint and redeem.`
      : `Can't reach Solana at <code>${RPC}</code>. Check your connection, or set <code>VITE_RPC_URL</code> to another RPC.`);
    return;
  }
  if (!reserve) {
    showNotice(`No Tender reserve found for program <code>${short(PROGRAM)}</code> on <code>${RPC}</code>. Deploy the program and run <code>scripts/init-reserve.ts</code> (see <a href="/docs/quickstart">Quickstart</a>).`);
    issuers = [];
  } else {
    $("#notice").classList.remove("show");
    vaults = await t.vaultBalances();
    try {
      issuers = await t.fetchIssuers();
      canList = true;
    } catch {
      // Keyless RPCs refuse getProgramAccounts. Show the dollars named in
      // the link or looked up before, and offer a search by symbol.
      canList = false;
      const want = route().tab === "mint" ? route().symbol : "";
      if (want) remember(want);
      const found = await Promise.all(known().map((s) => t.fetchIssuer(s).catch(() => null)));
      issuers = found.filter((x): x is IssuerState => !!x);
    }
    $("#find-form").hidden = canList;
    if (reserve.paused) showNotice("Minting is paused by the reserve admin. Redemptions still work.");
  }
  if (configProblems.length) showNotice(configProblems.join("<br>"));
  renderStats();
  renderIssuerSelect();
  renderDollars();
  await refreshBalances();
  renderAdmin();
}

// ---------------------------------------------------------------- dollars this browser knows about

let canList = true;
const KNOWN = "tender.known";
function known(): string[] {
  try { return JSON.parse(localStorage.getItem(KNOWN) || "[]"); } catch { return []; }
}
function remember(symbol: string) {
  const s = symbol.toUpperCase();
  if (!validSymbol(s)) return;
  try { localStorage.setItem(KNOWN, JSON.stringify([s, ...known().filter((x) => x !== s)].slice(0, 50))); } catch { /* storage off */ }
}

$("#find-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $<HTMLInputElement>("#find-symbol");
  const sym = input.value.trim().toUpperCase();
  if (!validSymbol(sym)) return toast("Symbols are 2-10 letters or digits", true);
  const i = await t.fetchIssuer(sym).catch(() => null);
  if (!i) return toast(`No dollar called ${sym} yet`, true);
  remember(sym);
  if (!issuers.some((x) => x.symbol === sym)) issuers.unshift(i);
  input.value = "";
  renderDollars();
  renderIssuerSelect();
});

function showNotice(html: string) {
  const n = $("#notice");
  n.innerHTML = html;
  n.classList.add("show");
}

function renderStats() {
  const r = reserve;
  const set = (id: string, v: string) => ($(id).textContent = v);
  if (!r) { ["#st-supply", "#st-value", "#st-usdc", "#st-tbill", "#st-yield", "#st-count"].forEach((i) => set(i, "–")); return; }
  set("#st-supply", "$" + formatUsd(r.totalSupply, 0));
  set("#st-value", "$" + formatUsd(reserveValue(r, vaults.usdc, vaults.tbill), 0));
  set("#st-usdc", "$" + formatUsd(vaults.usdc, 0));
  set("#st-tbill", "$" + formatUsd(reserveValue(r, 0n, vaults.tbill), 0));
  set("#st-yield", "$" + formatUsd(r.totalYield));
  set("#st-count", r.issuerCount.toString());
  $("#fee-note").textContent = `${100 - r.protocolFeeBps / 100}% of your share`;
}

// ---------------------------------------------------------------- issue

function addRoute(r?: Route) {
  const box = $("#routes");
  if (box.children.length >= 4) return;
  const row = document.createElement("div");
  row.className = "route";
  row.innerHTML = `
    <input class="input" data-k="wallet" placeholder="wallet address" />
    <input class="input" data-k="pct" inputmode="decimal" placeholder="%" />
    <select class="input" data-k="kind"><option value="0">as USDC</option><option value="1">as this dollar</option></select>
    <button type="button" class="x" aria-label="Remove route">&times;</button>`;
  (row.querySelector('[data-k="wallet"]') as HTMLInputElement).value = r ? r.wallet.toBase58() : "";
  (row.querySelector('[data-k="pct"]') as HTMLInputElement).value = r ? String(r.bps / 100) : "";
  (row.querySelector('[data-k="kind"]') as HTMLSelectElement).value = String(r?.kind ?? ROUTE_USDC);
  row.querySelector(".x")!.addEventListener("click", () => { row.remove(); routeTotal(); });
  row.addEventListener("input", routeTotal);
  box.appendChild(row);
  routeTotal();
}

function readRoutes(): Route[] {
  return [...document.querySelectorAll<HTMLElement>("#routes .route")].map((row, i) => {
    const w = (row.querySelector('[data-k="wallet"]') as HTMLInputElement).value.trim();
    let wallet: PublicKey;
    try { wallet = new PublicKey(w); } catch { throw new Error(`Route ${i + 1}: that's not a wallet address`); }
    const pct = Number((row.querySelector('[data-k="pct"]') as HTMLInputElement).value);
    const bps = Math.round(pct * 100);
    if (!(bps > 0)) throw new Error(`Route ${i + 1}: enter a share above 0%`);
    return { wallet, bps, kind: Number((row.querySelector('[data-k="kind"]') as HTMLSelectElement).value) };
  });
}

function routeTotal() {
  const pcts = [...document.querySelectorAll<HTMLInputElement>('#routes [data-k="pct"]')].map((i) => Number(i.value) || 0);
  const total = Math.round(pcts.reduce((a, b) => a + b, 0) * 100) / 100;
  const el = $("#route-total");
  el.textContent = `${total}%`;
  el.style.color = total === 100 ? "var(--green)" : "var(--red)";
  $<HTMLButtonElement>("#add-route").disabled = document.querySelectorAll("#routes .route").length >= 4;
}

$("#add-route").addEventListener("click", () => addRoute());
$("#f-symbol").addEventListener("input", (e) => {
  const i = e.target as HTMLInputElement;
  i.value = i.value.toUpperCase().replace(/[^A-Z0-9]/g, "");
  $("#coin-sym").textContent = i.value || "YOURUSD";
});

function startEdit(i: IssuerState) {
  editing = i;
  go("/issue");
  $("#issue-title").textContent = `Edit ${i.symbol} routes.`;
  $("#issue-sub").textContent = "Only the issuer's authority can change routes. Yield already earned goes out under the new routes at the next harvest.";
  $<HTMLInputElement>("#f-name").value = i.name;
  $<HTMLInputElement>("#f-symbol").value = i.symbol;
  $("#coin-sym").textContent = i.symbol;
  for (const id of ["#f-name", "#f-symbol", "#f-uri"]) $<HTMLInputElement>(id).disabled = true;
  $("#routes").innerHTML = "";
  i.routes.forEach((r) => addRoute(r));
  $("#issue-submit").textContent = "Save routes";
  $("#issue-cancel").style.display = "";
}
function stopEdit() {
  editing = null;
  $("#issue-title").textContent = "Issue a dollar.";
  $("#issue-sub").textContent = "One transaction creates the mint, its on-chain name and symbol, and your yield routes.";
  for (const id of ["#f-name", "#f-symbol", "#f-uri"]) { const el = $<HTMLInputElement>(id); el.disabled = false; el.value = ""; }
  $("#coin-sym").textContent = "YOURUSD";
  $("#routes").innerHTML = "";
  const k = walletKey();
  if (k) addRoute({ wallet: k, bps: 10_000, kind: ROUTE_USDC });
  $("#issue-submit").textContent = "Issue dollar";
  $("#issue-cancel").style.display = "none";
}
$("#issue-cancel").addEventListener("click", stopEdit);

$("#issue-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const btn = $<HTMLButtonElement>("#issue-submit");
  if (editing) {
    const target = editing;
    return run(btn, "Saving…", async () => {
      const me = needWallet();
      const sig = await signAndSend(conn, [t.setRoutes(me, target.address, readRoutes())]);
      stopEdit();
      return sig;
    });
  }
  run(btn, "Issuing…", async () => {
    const me = needWallet();
    needReserve();
    const name = $<HTMLInputElement>("#f-name").value.trim();
    const symbol = $<HTMLInputElement>("#f-symbol").value.trim();
    const uri = $<HTMLInputElement>("#f-uri").value.trim();
    if (!name) throw new Error("Give your dollar a name");
    if (!validSymbol(symbol)) throw new Error("Symbol must be 2-10 characters, A-Z and 0-9 only");
    if (await conn.getAccountInfo(t.issuerAddress(symbol))) throw new Error(`${symbol} is already taken`);
    const routes = readRoutes();
    if (routes.reduce((a, r) => a + r.bps, 0) !== 10_000) throw new Error("Route shares must add up to 100%");
    const sig = await signAndSend(conn, [t.createIssuer(me, { name, symbol, uri, routes })]);
    remember(symbol);
    go(`/mint/${symbol}`);
    return sig;
  });
});

// ---------------------------------------------------------------- convert

let mode: "mint" | "redeem" = "mint";
let bals = { usdc: 0n, dollar: 0n };

document.querySelectorAll<HTMLButtonElement>(".seg button").forEach((b) =>
  b.addEventListener("click", () => {
    mode = b.dataset.mode as "mint" | "redeem";
    document.querySelectorAll(".seg button").forEach((x) => x.classList.toggle("on", x === b));
    renderConvert();
  }),
);

function selectedIssuer() {
  return issuers.find((i) => i.symbol === $<HTMLSelectElement>("#c-issuer").value) ?? null;
}

function renderIssuerSelect() {
  const sel = $<HTMLSelectElement>("#c-issuer");
  const cur = sel.value || (route().tab === "mint" ? route().symbol : "");
  sel.innerHTML = issuers.length
    ? issuers.map((i) => `<option value="${i.symbol}">${i.symbol} · ${escapeHtml(i.name)}</option>`).join("")
    : `<option value="">No dollars issued yet</option>`;
  if (issuers.some((i) => i.symbol === cur)) sel.value = cur;
}
$("#c-issuer").addEventListener("change", () => {
  const sym = $<HTMLSelectElement>("#c-issuer").value;
  if (sym) history.replaceState(null, "", `/mint/${sym}`);
  refreshBalances();
});

async function tokenBal(acct: PublicKey) {
  try { return (await t.tokenBalances([acct]))[0]; } catch { return 0n; }
}

async function refreshBalances() {
  const me = walletKey();
  const i = selectedIssuer();
  if (!me || !reserve) { bals = { usdc: 0n, dollar: 0n }; renderConvert(); return; }
  const usdc = await tokenBal(t.usdcAta(me, reserve.usdcMint));
  const dollar = i ? await tokenBal(t.dollarAta(me, i.mint)) : 0n;
  bals = { usdc, dollar };
  const mine = await Promise.all(issuers.map(async (x) => [x.symbol, await tokenBal(t.dollarAta(me, x.mint))] as const));
  $("#my-bals").innerHTML =
    `<div class="row">USDC <b>${formatUsd(usdc)}</b></div>` +
    mine.map(([s, b]) => `<div class="row">${s} <b>${formatUsd(b)}</b></div>`).join("");
  renderConvert();
}

function renderConvert() {
  const sym = selectedIssuer()?.symbol ?? "dollar";
  const minting = mode === "mint";
  $("#c-label").textContent = minting ? "you pay (USDC)" : `you redeem (${sym})`;
  $("#c-get-label").textContent = minting ? `you get (${sym})` : "you get (USDC)";
  $("#c-submit").textContent = minting ? `Mint ${sym}` : `Redeem ${sym}`;
  $("#c-bal").textContent = walletKey() ? `balance ${formatUsd(minting ? bals.usdc : bals.dollar)} ${minting ? "USDC" : sym}` : "balance –";
  let get = "0.00";
  try { const v = $<HTMLInputElement>("#c-amount").value; if (v) get = formatUsd(parseUsd(v)); } catch { get = "–"; }
  $("#c-get").textContent = get;
  const cap = reserve?.maxSupply ?? 0n;
  $("#c-cap-row").hidden = !(minting && cap > 0n);
  if (reserve && cap > 0n) $("#c-cap").textContent = formatUsd(cap > reserve.totalSupply ? cap - reserve.totalSupply : 0n);
}
$("#c-amount").addEventListener("input", renderConvert);
$("#c-max").addEventListener("click", () => {
  $<HTMLInputElement>("#c-amount").value = formatUsd(mode === "mint" ? bals.usdc : bals.dollar, 6).replace(/,/g, "");
  renderConvert();
});

$("#convert-form").addEventListener("submit", (e) => {
  e.preventDefault();
  run($<HTMLButtonElement>("#c-submit"), mode === "mint" ? "Minting…" : "Redeeming…", async () => {
    const me = needWallet();
    const r = needReserve();
    const i = selectedIssuer();
    if (!i) throw new Error("Pick a dollar");
    const amount = parseUsd($<HTMLInputElement>("#c-amount").value);
    if (amount <= 0n) throw new Error("Enter an amount");
    if (mode === "mint" && r.maxSupply > 0n && r.totalSupply + amount > r.maxSupply)
      throw new Error(`The reserve's cap leaves room for ${formatUsd(r.maxSupply > r.totalSupply ? r.maxSupply - r.totalSupply : 0n)} more`);
    if (mode === "mint" && amount > bals.usdc) throw new Error("You don't have that much USDC");
    if (mode === "redeem" && amount > bals.dollar) throw new Error(`You don't have that much ${i.symbol}`);
    if (mode === "redeem" && amount > vaults.usdc) throw new Error("The reserve doesn't hold enough liquid USDC right now");
    const sig = await signAndSend(conn, t.convert(mode, me, r.usdcMint, i.symbol, amount));
    $<HTMLInputElement>("#c-amount").value = "";
    return sig;
  });
});

// ---------------------------------------------------------------- dollars

function escapeHtml(s: string) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));
}

function renderDollars() {
  const body = $("#dollar-rows");
  if (!reserve) { body.innerHTML = `<tr><td colspan="6" class="empty">No reserve on this cluster.</td></tr>`; return; }
  if (!issuers.length) {
    body.innerHTML = canList
      ? `<tr><td colspan="6" class="empty">No dollars yet. <a href="/issue">Issue the first one.</a></td></tr>`
      : `<tr><td colspan="6" class="empty">Find a dollar by its symbol above, or <a href="/issue">issue one</a>.</td></tr>`;
    return;
  }
  const me = walletKey();
  body.innerHTML = issuers.map((i, n) => {
    const ready = harvestable(reserve!, i, vaults.usdc, vaults.tbill);
    const routes = i.routes.map((r) => `${r.bps / 100}% ${r.kind === ROUTE_DOLLAR ? i.symbol : "USDC"} → ${short(r.wallet)}`).join("<br>");
    const mine = me && i.authority.equals(me);
    return `<tr>
      <td><div class="sym">${i.symbol}</div><div class="muted" style="font-size:13px">${escapeHtml(i.name)}</div></td>
      <td class="num">${formatUsd(i.supply)}</td>
      <td class="num">${formatUsd(ready, 6)}</td>
      <td class="num">${formatUsd(i.totalYieldPaid, 6)}</td>
      <td class="routes-mini">${routes}</td>
      <td style="white-space:nowrap">
        <button class="btn small yellow" data-harvest="${n}" ${ready > 0n ? "" : "disabled"}>Harvest</button>
        <a class="btn small ghost" href="/mint/${i.symbol}">Mint</a>
        ${mine ? `<button class="btn small ghost" data-edit="${n}">Edit routes</button>` : ""}
      </td></tr>`;
  }).join("");
  body.querySelectorAll<HTMLButtonElement>("[data-harvest]").forEach((b) =>
    b.addEventListener("click", () => run(b, "Harvesting…", async () => {
      const me = needWallet();
      return signAndSend(conn, t.harvest(me, needReserve().usdcMint, issuers[Number(b.dataset.harvest)]));
    })),
  );
  body.querySelectorAll<HTMLButtonElement>("[data-edit]").forEach((b) =>
    b.addEventListener("click", () => startEdit(issuers[Number(b.dataset.edit)])),
  );
}

// ---------------------------------------------------------------- admin

const PRICE_SCALE = 1_000_000_000n;
let refPrice: number | null = null;
let myTbill = 0n, myUsdc = 0n;

const priceToNum = (p: bigint) => Number(p) / 1e9;
const numToPrice = (n: number) => BigInt(Math.round(n * 1e9));

/** Live market price of the Treasury token from Jupiter, in USD. */
async function loadRefPrice(mint: PublicKey) {
  try {
    const r = await fetch(`https://lite-api.jup.ag/price/v3?ids=${mint.toBase58()}`);
    const j = await r.json();
    const p = Number(j[mint.toBase58()]?.usdPrice);
    refPrice = p > 0 ? p : null;
  } catch { refPrice = null; }
}
/** The price an update may set right now, closest to the market price. */
function suggestedPrice(r: ReserveState): bigint | null {
  if (refPrice == null) return null;
  const lo = (r.tbillPrice * 9950n) / 10_000n, hi = (r.tbillPrice * 10050n) / 10_000n;
  const want = numToPrice(refPrice);
  return want < lo ? lo : want > hi ? hi : want;
}

async function renderAdmin() {
  const me = walletKey();
  const r = reserve;
  const roles = {
    admin: !!(me && r && r.admin.equals(me)),
    price: !!(me && r && r.priceAuthority.equals(me)),
    allocator: !!(me && r && r.allocator.equals(me)),
  };
  const any = roles.admin || roles.price || roles.allocator;
  $("#admin-tab").hidden = !any;
  if (!any || !r) return;
  document.querySelectorAll<HTMLElement>("#pane-admin [data-role]").forEach((el) => {
    const role = el.dataset.role!;
    el.hidden = role === "price" ? !roles.price : role === "allocator" ? !roles.allocator : role === "admin" ? !roles.admin : false;
  });
  document.querySelectorAll(".tb-sym").forEach((e) => (e.textContent = TBILL_SYMBOL));
  ($("#ad-jup") as HTMLAnchorElement).href = `https://jup.ag/swap/${r.usdcMint.toBase58()}-${r.tbillMint.toBase58()}`;

  // price
  if (refPrice == null) await loadRefPrice(r.tbillMint);
  $("#ad-price").textContent = `$${priceToNum(r.tbillPrice).toFixed(6)}`;
  const updated = new Date(Number(r.priceUpdatedAt) * 1000);
  const next = new Date((Number(r.priceUpdatedAt) + 3600) * 1000);
  $("#ad-updated").textContent = updated.toLocaleString() + (next > new Date() ? ` · next after ${next.toLocaleTimeString()}` : "");
  $("#ad-ref").textContent = refPrice == null ? "unavailable" : `$${refPrice.toFixed(6)}`;
  $("#ad-range").textContent = `$${priceToNum((r.tbillPrice * 9950n) / 10_000n).toFixed(6)} – $${priceToNum((r.tbillPrice * 10050n) / 10_000n).toFixed(6)}`;
  const priceInput = $<HTMLInputElement>("#ad-new-price");
  const sug = suggestedPrice(r);
  if (!priceInput.value && sug) priceInput.value = priceToNum(sug).toFixed(6);

  // treasuries
  $("#ad-v-usdc").textContent = formatUsd(vaults.usdc);
  $("#ad-v-tbill").textContent = `${formatUsd(vaults.tbill)} ($${formatUsd(reserveValue(r, 0n, vaults.tbill))})`;
  const floor = (r.totalSupply * BigInt(r.minLiquidBps)) / 10_000n;
  $("#ad-floor").textContent = `${formatUsd(floor)} (${r.minLiquidBps / 100}% of supply)`;
  if (me) {
    myUsdc = await tokenBal(t.usdcAta(me, r.usdcMint));
    myTbill = await tokenBal(getAssociatedTokenAddressSync(r.tbillMint, me, true));
  }
  $("#ad-my-usdc").textContent = formatUsd(myUsdc);
  $("#ad-my-tbill").textContent = formatUsd(myTbill);
  renderTbillUsd();

  // fee
  $("#ad-owed").textContent = formatUsd(r.protocolOwed, 6);
  $("#ad-treasury").textContent = short(r.treasury);

  // settings (only fill once so edits aren't overwritten by refreshes)
  const fee = $<HTMLInputElement>("#ad-fee");
  if (!fee.dataset.filled) {
    fee.dataset.filled = "1";
    fee.value = String(r.protocolFeeBps / 100);
    $<HTMLInputElement>("#ad-minliq").value = String(r.minLiquidBps / 100);
    $<HTMLInputElement>("#ad-cap").value = formatUsd(r.maxSupply, 0).replace(/,/g, "");
    $<HTMLInputElement>("#ad-pa").value = r.priceAuthority.toBase58();
    $<HTMLInputElement>("#ad-alloc").value = r.allocator.toBase58();
    $<HTMLInputElement>("#ad-tre").value = r.treasury.toBase58();
    $<HTMLInputElement>("#ad-paused").checked = r.paused;
  }
}

function tbillAmount(): bigint {
  return parseUsd($<HTMLInputElement>("#ad-tbill-amt").value || "0");
}
function renderTbillUsd() {
  if (!reserve) return;
  let v = 0n;
  try { v = (tbillAmount() * reserve.tbillPrice) / PRICE_SCALE; } catch { /* ignore */ }
  $("#ad-tbill-usd").textContent = `= ${formatUsd(v)} USDC at the reserve price`;
}
$("#ad-tbill-amt").addEventListener("input", renderTbillUsd);
$("#ad-tbill-max").addEventListener("click", () => {
  const r = reserve;
  if (!r) return;
  const floor = (r.totalSupply * BigInt(r.minLiquidBps)) / 10_000n;
  const room = vaults.usdc > floor ? vaults.usdc - floor : 0n;
  let amt = (room * PRICE_SCALE) / r.tbillPrice;
  if (amt > myTbill) amt = myTbill;
  $<HTMLInputElement>("#ad-tbill-amt").value = formatUsd(amt, 6).replace(/,/g, "");
  renderTbillUsd();
});

$("#price-form").addEventListener("submit", (e) => {
  e.preventDefault();
  run($<HTMLButtonElement>("#ad-price-btn"), "Updating…", async () => {
    const me = needWallet();
    const r = needReserve();
    const n = Number($<HTMLInputElement>("#ad-new-price").value);
    if (!(n > 0)) throw new Error("Enter a price like 1.152");
    const p = numToPrice(n);
    const diff = p > r.tbillPrice ? p - r.tbillPrice : r.tbillPrice - p;
    if (diff * 10_000n > r.tbillPrice * 50n) throw new Error("That's more than 0.5% from the current price. Step towards it over several updates.");
    if (Date.now() / 1000 - Number(r.priceUpdatedAt) < 3600) throw new Error("The price was updated less than an hour ago");
    const sig = await signAndSend(conn, [t.updatePrice(me, p)]);
    $<HTMLInputElement>("#ad-new-price").value = "";
    return sig;
  });
});

for (const kind of ["invest", "divest"] as const) {
  $(`#ad-${kind}`).addEventListener("click", () =>
    run($<HTMLButtonElement>(`#ad-${kind}`), kind === "invest" ? "Investing…" : "Divesting…", async () => {
      const me = needWallet();
      const r = needReserve();
      const amt = tbillAmount();
      if (amt <= 0n) throw new Error(`Enter an amount of ${TBILL_SYMBOL}`);
      const usd = (amt * r.tbillPrice) / PRICE_SCALE;
      if (kind === "invest" && amt > myTbill) throw new Error(`You hold ${formatUsd(myTbill)} ${TBILL_SYMBOL}`);
      if (kind === "divest" && usd + 1n > myUsdc) throw new Error(`Divesting needs ${formatUsd(usd + 1n)} USDC in your wallet`);
      if (kind === "divest" && amt > vaults.tbill) throw new Error(`The reserve holds ${formatUsd(vaults.tbill)} ${TBILL_SYMBOL}`);
      const sig = await signAndSend(conn, t.rebalanceIxs(kind, me, r, amt));
      $<HTMLInputElement>("#ad-tbill-amt").value = "";
      return sig;
    }),
  );
}

$("#ad-collect").addEventListener("click", () =>
  run($<HTMLButtonElement>("#ad-collect"), "Collecting…", async () => {
    const me = needWallet();
    const r = needReserve();
    return signAndSend(conn, t.collectProtocolFeeIxs(me, r));
  }),
);

$("#settings-form").addEventListener("submit", (e) => {
  e.preventDefault();
  run($<HTMLButtonElement>("#ad-save"), "Saving…", async () => {
    const me = needWallet();
    const r = needReserve();
    const pct = (id: string, max: number) => {
      const n = Number($<HTMLInputElement>(id).value);
      if (!(n >= 0 && n <= max)) throw new Error(`Enter a percentage from 0 to ${max}`);
      return Math.round(n * 100);
    };
    const key = (id: string) => {
      try { return new PublicKey($<HTMLInputElement>(id).value.trim()); } catch { throw new Error("One of the wallet addresses isn't valid"); }
    };
    const sig = await signAndSend(conn, [t.updateReserve(me, {
      ...t.currentParams(r),
      protocolFeeBps: pct("#ad-fee", 20),
      minLiquidBps: pct("#ad-minliq", 100),
      maxSupply: parseUsd($<HTMLInputElement>("#ad-cap").value || "0"),
      priceAuthority: key("#ad-pa"),
      allocator: key("#ad-alloc"),
      treasury: key("#ad-tre"),
      paused: $<HTMLInputElement>("#ad-paused").checked,
    })]);
    delete $<HTMLInputElement>("#ad-fee").dataset.filled;
    return sig;
  });
});

// ---------------------------------------------------------------- boot

$("#devnet-help").hidden = CLUSTER !== "devnet";
upgradeOldLink();
showTab();
routeTotal();
connectWallet(true).catch(() => null).then(onWallet);
refresh().then(showTab);
setInterval(() => { if (!document.hidden) refresh(); }, 15_000);
