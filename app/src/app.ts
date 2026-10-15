// The app page: issue a dollar, mint & redeem, and harvest yield, against
// the program on whatever cluster VITE_RPC_URL points at.

import "./polyfill";
import "./styles.css";

import { Connection, PublicKey } from "@solana/web3.js";
import {
  DEFAULT_PROGRAM_ID, ROUTE_DOLLAR, ROUTE_USDC, Tender, formatUsd, friendlyError, harvestable, parseUsd,
  reserveValue, validSymbol, type IssuerState, type ReserveState, type Route,
} from "./chain/tender";
import { connectWallet, disconnectWallet, signAndSend, walletKey } from "./chain/wallet";

const RPC = import.meta.env.VITE_RPC_URL || "https://api.devnet.solana.com";
const CLUSTER = import.meta.env.VITE_CLUSTER || "devnet";
const PROGRAM = import.meta.env.VITE_PROGRAM_ID ? new PublicKey(import.meta.env.VITE_PROGRAM_ID) : DEFAULT_PROGRAM_ID;

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
function toast(html: string, err = false) {
  const el = $("#toast");
  el.innerHTML = html;
  el.className = "toast show" + (err ? " err" : "");
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (el.className = "toast"), err ? 6000 : 4500);
}

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

function showTab() {
  const tab = (location.hash.slice(1).split("?")[0] || "issue") as string;
  const name = ["issue", "convert", "dollars"].includes(tab) ? tab : "issue";
  document.querySelectorAll(".pane").forEach((p) => p.classList.toggle("on", p.id === `pane-${name}`));
  document.querySelectorAll("[data-tab]").forEach((a) => a.classList.toggle(a.closest(".tabs") ? "on" : "active", (a as HTMLElement).dataset.tab === name));
  const q = new URLSearchParams(location.hash.split("?")[1] || "");
  if (q.get("symbol") && name === "issue" && !editing) {
    $<HTMLInputElement>("#f-symbol").value = q.get("symbol")!.toUpperCase().slice(0, 10);
    $("#coin-sym").textContent = $<HTMLInputElement>("#f-symbol").value;
  }
  if (q.get("dollar") && name === "convert") $<HTMLSelectElement>("#c-issuer").value = q.get("dollar")!;
}
window.addEventListener("hashchange", showTab);

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
    showNotice(`No Tender reserve found for program <code>${short(PROGRAM)}</code> on <code>${RPC}</code>. Deploy the program and run <code>scripts/init-reserve.ts</code> (see <a href="/docs.html#quickstart">Quickstart</a>).`);
    issuers = [];
  } else {
    $("#notice").classList.remove("show");
    [vaults, issuers] = await Promise.all([t.vaultBalances(), t.fetchIssuers()]);
    if (reserve.paused) showNotice("Minting is paused by the reserve admin. Redemptions still work.");
  }
  renderStats();
  renderIssuerSelect();
  renderDollars();
  await refreshBalances();
}

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
  location.hash = "issue";
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
    location.hash = `convert?dollar=${symbol}`;
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
  const cur = sel.value || new URLSearchParams(location.hash.split("?")[1] || "").get("dollar") || "";
  sel.innerHTML = issuers.length
    ? issuers.map((i) => `<option value="${i.symbol}">${i.symbol} · ${escapeHtml(i.name)}</option>`).join("")
    : `<option value="">No dollars issued yet</option>`;
  if (issuers.some((i) => i.symbol === cur)) sel.value = cur;
}
$("#c-issuer").addEventListener("change", () => refreshBalances());

async function tokenBal(acct: PublicKey) {
  try { return BigInt((await conn.getTokenAccountBalance(acct)).value.amount); } catch { return 0n; }
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
  if (!issuers.length) { body.innerHTML = `<tr><td colspan="6" class="empty">No dollars yet. <a href="#issue">Issue the first one.</a></td></tr>`; return; }
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
        <a class="btn small ghost" href="#convert?dollar=${i.symbol}">Mint</a>
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

// ---------------------------------------------------------------- boot

$("#devnet-help").hidden = CLUSTER !== "devnet";
showTab();
routeTotal();
connectWallet(true).catch(() => null).then(onWallet);
refresh().then(showTab);
setInterval(() => { if (!document.hidden) refresh(); }, 15_000);
