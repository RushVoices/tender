// Landing page: the 3D coin-and-reserve scene and a live simulation of the
// reserve that runs the same yield math as the program (see
// `projectedIndex` in chain/tender.ts, which mirrors Reserve::accrue).

import "./polyfill";
import "./styles.css";
import * as THREE from "three";
import { RoomEnvironment } from "three/examples/jsm/environments/RoomEnvironment.js";
import { RoundedBoxGeometry } from "three/examples/jsm/geometries/RoundedBoxGeometry.js";
import { INDEX_SCALE, PRICE_SCALE, formatUsd, projectedIndex, type ReserveState } from "./chain/tender";

const $ = <T extends HTMLElement = HTMLElement>(s: string) => document.querySelector(s) as T;
const USD = 1_000_000n;

// ---------------------------------------------------------------- ticker

const tickerInput = $<HTMLInputElement>("#ticker");
let ticker = "YOURUSD";
function cleanTicker(v: string) { return v.toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 10); }
tickerInput.addEventListener("input", () => {
  const c = cleanTicker(tickerInput.value);
  if (c !== tickerInput.value) tickerInput.value = c;
  ticker = c || "YOURUSD";
  document.querySelectorAll("[data-sym]").forEach((el) => (el.textContent = ticker));
  document.querySelectorAll<HTMLAnchorElement>('a[href^="app.html#issue"]').forEach((a) => (a.href = `app.html#issue?symbol=${ticker}`));
  drawFace();
});

// ---------------------------------------------------------------- simulation
// One simulated week passes per second. Treasuries earn ~4.6% a year.

const sim = {
  usdc: 0n, tbill: 0n, price: PRICE_SCALE, supply: 0n, unclaimed: 0n, index: 0n,
  snapshot: 0n, accrued: 0n, routed: [0n, 0n], protocol: 0n, totalYield: 0n,
};
const FEE_BPS = 1000n;
const ROUTE_BPS = [6000n, 4000n];
const WEEKLY_GROWTH = 1_000_870_000n; // (1.046)^(1/52) in PRICE_SCALE units

function asReserve(): ReserveState {
  return {
    tbillPrice: sim.price, totalSupply: sim.supply, unclaimedYield: sim.unclaimed, yieldIndex: sim.index,
    protocolFeeBps: Number(FEE_BPS),
  } as ReserveState;
}
const tbillValue = () => (sim.tbill * sim.price) / PRICE_SCALE;
const value = () => sim.usdc + tbillValue();

/** Reserve::accrue + Issuer::settle for our single example issuer. */
function accrue() {
  const newIndex = projectedIndex(asReserve(), sim.usdc, sim.tbill);
  if (newIndex !== sim.index && sim.supply > 0n) {
    const owed = sim.supply + sim.unclaimed;
    const surplus = value() - owed;
    const fee = (surplus * FEE_BPS) / 10_000n;
    sim.protocol += fee;
    sim.unclaimed += surplus;
    sim.totalYield += surplus;
    sim.index = newIndex;
  }
  sim.accrued += (sim.supply * (sim.index - sim.snapshot)) / INDEX_SCALE;
  sim.snapshot = sim.index;
}

/** Allocator keeps ~25% liquid, the rest in Treasuries (at price, value-neutral). */
function rebalance() {
  const target = (sim.supply * 25n) / 100n;
  if (sim.usdc > target + 50n * USD) {
    const usdcOut = sim.usdc - target;
    const t = (usdcOut * PRICE_SCALE) / sim.price;
    sim.tbill += t;
    sim.usdc -= (t * sim.price) / PRICE_SCALE;
  } else if (sim.usdc < (target * 6n) / 10n && sim.tbill > 0n) {
    const need = target - sim.usdc;
    let t = (need * PRICE_SCALE) / sim.price;
    if (t > sim.tbill) t = sim.tbill;
    sim.tbill -= t;
    sim.usdc += (t * sim.price + PRICE_SCALE - 1n) / PRICE_SCALE;
  }
}

function simMint(amount: bigint) {
  accrue();
  sim.usdc += amount; sim.supply += amount;
  rebalance();
  say("Minted", `${formatUsd(amount, 0)} USDC went into the reserve and ${formatUsd(amount, 0)} ${ticker} came out. Supply and the reserve grow together, so nothing counts as yield.`);
  scene?.dropCoin(Number(amount / USD));
}
function simRedeem(amount: bigint) {
  if (sim.supply < amount) return say("Nothing to redeem", `Mint some ${ticker} first.`);
  accrue();
  if (sim.usdc < amount) {
    // the allocator divests just enough Treasuries to cover it
    const t = ((amount - sim.usdc) * PRICE_SCALE + sim.price - 1n) / sim.price;
    sim.tbill -= t;
    sim.usdc += (t * sim.price) / PRICE_SCALE;
  }
  sim.supply -= amount; sim.usdc -= amount;
  rebalance();
  say("Redeemed", `${formatUsd(amount, 0)} ${ticker} burned, ${formatUsd(amount, 0)} USDC paid out at par. Redemptions can't be paused.`);
  scene?.liftCoin();
}
function simHarvest() {
  accrue();
  if (sim.accrued === 0n) return say("Nothing yet", "Yield builds up as the Treasuries earn. Give it a few seconds.");
  const pay = sim.accrued;
  const r0 = (pay * ROUTE_BPS[0]) / 10_000n;
  sim.routed[0] += r0; sim.routed[1] += pay - r0;
  // route 0 pays USDC; route 1 mints new dollars backed by the yield
  sim.usdc -= r0;
  sim.supply += pay - r0;
  sim.unclaimed -= pay;
  sim.accrued = 0n;
  rebalance();
  say("Harvested", `${formatUsd(pay, 4)} of yield paid out: 60% as USDC to the treasury wallet, 40% minted as ${ticker} to the holders wallet.`);
  pulseChip("r0"); pulseChip("r1");
  scene?.burst();
}

function say(title: string, text: string) {
  $("#p-title").textContent = title;
  $("#p-text").textContent = text;
}
function pulseChip(k: string) {
  const el = document.querySelector(`[data-chip="${k}"]`);
  el?.classList.add("pulse");
  setTimeout(() => el?.classList.remove("pulse"), 450);
}

$("#sim-mint").addEventListener("click", () => simMint(1_000n * USD));
$("#sim-redeem").addEventListener("click", () => simRedeem(250n * USD));
$("#sim-harvest").addEventListener("click", simHarvest);

let lastWeek = performance.now();
function tickSim(now: number) {
  if (now - lastWeek > 1000) {
    lastWeek = now;
    sim.price = (sim.price * WEEKLY_GROWTH) / PRICE_SCALE;
    if (sim.tbill > 0n) pulseChip("tbill");
  }
}

function render() {
  accrue();
  const pending = sim.accrued;
  const v = value();
  const set = (id: string, s: string) => { const el = $(id); if (el.textContent !== s) el.textContent = s; };
  set("#s-supply", formatUsd(sim.supply));
  set("#s-value", formatUsd(v));
  set("#s-usdc", formatUsd(sim.usdc));
  set("#s-tbill", formatUsd(tbillValue()));
  set("#s-yield", formatUsd(sim.totalYield, 6));
  set("#s-r0", formatUsd(sim.routed[0], 4));
  set("#s-r1", formatUsd(sim.routed[1], 4));
  const backing = sim.supply ? Number((v * 1_000_000n) / sim.supply) / 1e6 : 1;
  set("#s-backing", backing.toFixed(4));
  const tot = Number(v) || 1;
  $("#b-usdc").style.width = `${(Number(sim.usdc) / tot) * 100}%`;
  $("#b-tbill").style.width = `${(Number(tbillValue()) / tot) * 100}%`;

  // reserve band
  const owed = sim.supply;
  const surplus = v > owed ? v - owed : 0n;
  set("#l-owed", formatUsd(owed));
  set("#l-held", formatUsd(v));
  set("#l-surplus", formatUsd(surplus, 6));
  const u = Number(sim.usdc), t = Number(tbillValue()) - Number(surplus), y = Number(surplus);
  const sum = u + Math.max(t, 0) + y || 1;
  $("#sb-u").style.flex = String(sum === 1 ? 1 : u / sum);
  $("#sb-t").style.flex = String(Math.max(t, 0) / sum);
  $("#sb-y").style.flex = String(Math.max(y / sum, pending > 0n ? 0.04 : 0));
  set("#sb-t", t / sum > 0.12 ? "Treasuries" : "");
  set("#sb-y", y > 0 ? "yield" : "");
  set("#sb-u", u / sum > 0.12 ? "USDC" : "");

  scene?.setStacks(Number(sim.usdc / USD), Number(tbillValue() / USD));
}

// ---------------------------------------------------------------- 3D scene

const face = document.createElement("canvas");
face.width = face.height = 1024;

function drawFace() {
  const g = face.getContext("2d")!;
  const c = 512;
  const grad = g.createRadialGradient(c - 180, c - 220, 60, c, c, 560);
  grad.addColorStop(0, "#fff2a0");
  grad.addColorStop(0.45, "#ffd60a");
  grad.addColorStop(1, "#e9a800");
  g.fillStyle = grad;
  g.fillRect(0, 0, 1024, 1024);
  // rims
  g.strokeStyle = "#111110";
  g.lineWidth = 26;
  g.beginPath(); g.arc(c, c, 470, 0, Math.PI * 2); g.stroke();
  g.lineWidth = 6;
  g.beginPath(); g.arc(c, c, 420, 0, Math.PI * 2); g.stroke();
  // dotted inner ring
  g.fillStyle = "#111110";
  for (let i = 0; i < 72; i++) {
    const a = (i / 72) * Math.PI * 2;
    g.beginPath(); g.arc(c + Math.cos(a) * 392, c + Math.sin(a) * 392, 5, 0, Math.PI * 2); g.fill();
  }
  // curved legend
  const text = "ONE DOLLAR · BACKED 1:1 · USDC + TREASURIES · ";
  g.font = "700 38px 'JetBrains Mono', monospace";
  g.textAlign = "center"; g.textBaseline = "middle";
  const chars = [...text];
  chars.forEach((ch, i) => {
    const a = (i / chars.length) * Math.PI * 2 - Math.PI / 2;
    g.save();
    g.translate(c + Math.cos(a) * 340, c + Math.sin(a) * 340);
    g.rotate(a + Math.PI / 2);
    g.fillText(ch, 0, 0);
    g.restore();
  });
  // ticker
  const t = ticker;
  let size = 230;
  g.font = `900 ${size}px 'Inter Tight', sans-serif`;
  while (g.measureText(t).width > 520 && size > 60) { size -= 6; g.font = `900 ${size}px 'Inter Tight', sans-serif`; }
  g.save();
  g.translate(c, c + 6);
  (g as CanvasRenderingContext2D & { letterSpacing?: string }).letterSpacing = `${-size * 0.05}px`;
  g.fillStyle = "rgba(255,255,255,.55)";
  g.fillText(t, 3, 4);
  g.fillStyle = "#111110";
  g.fillText(t, 0, 0);
  g.restore();
  g.font = "700 40px 'JetBrains Mono', monospace";
  g.fillText("$1", c, c - 190);
  g.font = "600 30px 'JetBrains Mono', monospace";
  g.fillText("TENDER", c, c + 196);
  faceTex.needsUpdate = true;
  backTex.needsUpdate = true;
}
const faceTex = new THREE.CanvasTexture(face);
faceTex.colorSpace = THREE.SRGBColorSpace;
faceTex.anisotropy = 8;
faceTex.center.set(0.5, 0.5);
faceTex.rotation = Math.PI / 2;
// the bottom cap needs its own texture object so both caps read upright
const backTex = faceTex.clone();

function smallFace(kind: "usdc" | "dollar") {
  const cv = document.createElement("canvas");
  cv.width = cv.height = 256;
  const g = cv.getContext("2d")!;
  g.fillStyle = kind === "usdc" ? "#ffffff" : "#ffd60a";
  g.fillRect(0, 0, 256, 256);
  g.strokeStyle = kind === "usdc" ? "#2775ca" : "#111";
  g.lineWidth = 16;
  g.beginPath(); g.arc(128, 128, 104, 0, Math.PI * 2); g.stroke();
  g.fillStyle = kind === "usdc" ? "#2775ca" : "#111";
  g.font = "900 96px 'Inter Tight', sans-serif";
  g.textAlign = "center"; g.textBaseline = "middle";
  g.fillText("$", 128, 134);
  const tex = new THREE.CanvasTexture(cv);
  tex.colorSpace = THREE.SRGBColorSpace;
  return tex;
}

function reedTexture() {
  const cv = document.createElement("canvas");
  cv.width = 512; cv.height = 16;
  const g = cv.getContext("2d")!;
  for (let x = 0; x < 512; x += 4) {
    g.fillStyle = x % 8 ? "#f6c400" : "#c99600";
    g.fillRect(x, 0, 4, 16);
  }
  const tex = new THREE.CanvasTexture(cv);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.wrapS = THREE.RepeatWrapping;
  tex.repeat.set(6, 1);
  return tex;
}

class Scene {
  renderer: THREE.WebGLRenderer;
  scene = new THREE.Scene();
  camera = new THREE.PerspectiveCamera(30, 1, 0.1, 100);
  root = new THREE.Group();
  hero = new THREE.Group();
  usdcStack = new THREE.Group();
  tbillStack = new THREE.Group();
  falling: { mesh: THREE.Mesh; v: number; target: number; spin: number }[] = [];
  lifting: { mesh: THREE.Mesh; t: number }[] = [];
  sparks: { mesh: THREE.Mesh; t: number; from: THREE.Vector3; to: THREE.Vector3 }[] = [];
  usdcCoinGeo = new THREE.CylinderGeometry(0.42, 0.42, 0.09, 48);
  usdcMats: THREE.Material[];
  dollarMats: THREE.Material[];
  barGeo = new RoundedBoxGeometry(1.0, 0.16, 0.48, 3, 0.04);
  barMat = new THREE.MeshStandardMaterial({ color: 0xffe37a, metalness: 0.65, roughness: 0.3 });
  sparkGeo = new THREE.SphereGeometry(0.05, 12, 12);
  sparkMat = new THREE.MeshBasicMaterial({ color: 0xffc400 });
  spin = 0.35; // rad/s, idle
  vel = 0;
  dragging = false;
  lastX = 0;
  tilt = new THREE.Vector2();
  shown = { usdc: -1, tbill: -1 };

  constructor(private host: HTMLElement) {
    this.renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
    this.renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
    this.renderer.shadowMap.enabled = true;
    this.renderer.shadowMap.type = THREE.PCFSoftShadowMap;
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.05;
    host.prepend(this.renderer.domElement);

    const pmrem = new THREE.PMREMGenerator(this.renderer);
    this.scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;

    this.camera.position.set(0, 1.6, 11.5);
    this.camera.lookAt(0, 0.35, 0);

    const key = new THREE.DirectionalLight(0xffffff, 1.6);
    key.position.set(3.5, 7, 4.5);
    key.castShadow = true;
    key.shadow.mapSize.set(2048, 2048);
    key.shadow.camera.left = -5; key.shadow.camera.right = 5;
    key.shadow.camera.top = 5; key.shadow.camera.bottom = -5;
    key.shadow.radius = 6;
    key.shadow.bias = -0.0008;
    this.scene.add(key, new THREE.HemisphereLight(0xffffff, 0xfff1c4, 0.5));

    const floor = new THREE.Mesh(new THREE.PlaneGeometry(30, 30), new THREE.ShadowMaterial({ opacity: 0.13 }));
    floor.rotation.x = -Math.PI / 2;
    floor.position.y = -2.05;
    floor.receiveShadow = true;
    this.scene.add(floor);

    // the reserve plinth
    const plinth = new THREE.Mesh(
      new RoundedBoxGeometry(4.6, 0.55, 2.3, 6, 0.14),
      new THREE.MeshStandardMaterial({ color: 0xffffff, roughness: 0.55, metalness: 0.0 }),
    );
    plinth.position.y = -1.77;
    plinth.castShadow = plinth.receiveShadow = true;
    const stripe = new THREE.Mesh(
      new RoundedBoxGeometry(4.62, 0.09, 2.32, 2, 0.04),
      new THREE.MeshStandardMaterial({ color: 0xffd60a, roughness: 0.4 }),
    );
    stripe.position.y = -1.62;
    this.root.add(plinth, stripe);
    this.usdcStack.position.set(-1.1, -1.495, 0.1);
    this.tbillStack.position.set(1.05, -1.495, 0.1);
    this.root.add(this.usdcStack, this.tbillStack);

    // hero coin
    const edge = new THREE.MeshStandardMaterial({ map: reedTexture(), metalness: 0.75, roughness: 0.28 });
    const faceMat = new THREE.MeshStandardMaterial({ map: faceTex, metalness: 0.45, roughness: 0.32 });
    const backMat = new THREE.MeshStandardMaterial({ map: backTex, metalness: 0.45, roughness: 0.32 });
    const coin = new THREE.Mesh(new THREE.CylinderGeometry(1.45, 1.45, 0.24, 128, 1), [edge, faceMat, backMat]);
    coin.rotation.x = Math.PI / 2;
    coin.castShadow = true;
    this.hero.add(coin);
    this.hero.position.y = 1.15;
    this.root.add(this.hero);

    const mk = (kind: "usdc" | "dollar") => {
      const f = new THREE.MeshStandardMaterial({ map: smallFace(kind), metalness: kind === "usdc" ? 0.1 : 0.45, roughness: 0.35 });
      const e = new THREE.MeshStandardMaterial({ color: kind === "usdc" ? 0xe9eef6 : 0xf2b705, metalness: 0.5, roughness: 0.3 });
      return [e, f, f];
    };
    this.usdcMats = mk("usdc");
    this.dollarMats = mk("dollar");

    this.scene.add(this.root);
    this.resize();
    new ResizeObserver(() => this.resize()).observe(host);
    this.bindPointer();
  }

  resize() {
    const w = this.host.clientWidth, h = this.host.clientHeight;
    this.renderer.setSize(w, h, false);
    this.camera.aspect = w / h;
    // keep the whole scene in frame on narrow stages
    this.camera.position.z = w / h < 0.9 ? 14.5 : 11.5;
    this.camera.updateProjectionMatrix();
  }

  bindPointer() {
    const el = this.renderer.domElement;
    el.addEventListener("pointerdown", (e) => { this.dragging = true; this.lastX = e.clientX; el.setPointerCapture(e.pointerId); });
    el.addEventListener("pointerup", () => (this.dragging = false));
    el.addEventListener("pointercancel", () => (this.dragging = false));
    el.addEventListener("pointermove", (e) => {
      const r = el.getBoundingClientRect();
      this.tilt.set(((e.clientX - r.left) / r.width - 0.5) * 2, ((e.clientY - r.top) / r.height - 0.5) * 2);
      if (!this.dragging) return;
      const dx = e.clientX - this.lastX;
      this.lastX = e.clientX;
      this.hero.rotation.y += dx * 0.012;
      this.vel = dx * 0.6;
    });
    el.addEventListener("pointerleave", () => this.tilt.set(0, 0));
    el.addEventListener("dblclick", () => simMint(1_000n * USD));
  }

  /** Rebuild the stacks when their sizes change (one coin per 250 USDC, one bar per 500). */
  setStacks(usdc: number, tbill: number) {
    const nU = Math.min(Math.ceil(usdc / 250), 22);
    const nT = Math.min(Math.ceil(tbill / 500), 14);
    if (nU !== this.shown.usdc) {
      this.fill(this.usdcStack, nU, () => {
        const m = new THREE.Mesh(this.usdcCoinGeo, this.usdcMats);
        m.castShadow = true;
        return m;
      }, 0.095);
      this.shown.usdc = nU;
    }
    if (nT !== this.shown.tbill) {
      this.fill(this.tbillStack, nT, () => {
        const m = new THREE.Mesh(this.barGeo, this.barMat);
        m.castShadow = true;
        return m;
      }, 0.165);
      this.shown.tbill = nT;
    }
  }

  fill(group: THREE.Group, n: number, make: () => THREE.Mesh, step: number) {
    while (group.children.length > n) group.remove(group.children[group.children.length - 1]);
    while (group.children.length < n) {
      const i = group.children.length;
      const m = make();
      m.position.set(Math.sin(i * 1.7) * 0.025, step / 2 + i * step, Math.cos(i * 2.3) * 0.025);
      if (group === this.tbillStack) m.rotation.y = (i % 2 ? 1 : -1) * 0.06;
      group.add(m);
    }
  }

  dropCoin(amount: number) {
    const count = Math.min(4, Math.max(1, Math.round(amount / 400)));
    for (let k = 0; k < count; k++) {
      const m = new THREE.Mesh(this.usdcCoinGeo, this.dollarMats);
      m.castShadow = true;
      m.position.set(-1.1 + (Math.random() - 0.5) * 0.2, 0.2 + k * 0.5, 0.1);
      m.rotation.set(Math.random() * 2, 0, Math.random());
      this.root.add(m);
      this.falling.push({ mesh: m, v: 0, target: -1.45 + this.usdcStack.children.length * 0.095, spin: 4 + Math.random() * 3 });
    }
    this.vel += 6;
  }

  liftCoin() {
    const top = this.usdcStack.children[this.usdcStack.children.length - 1] as THREE.Mesh | undefined;
    const m = new THREE.Mesh(this.usdcCoinGeo, this.usdcMats.map((x) => {
      const c = (x as THREE.MeshStandardMaterial).clone(); c.transparent = true; return c;
    }));
    const p = top ? top.getWorldPosition(new THREE.Vector3()) : new THREE.Vector3(-1.1, -1.4, 0.1);
    this.root.worldToLocal(p);
    m.position.copy(p);
    this.root.add(m);
    this.lifting.push({ mesh: m, t: 0 });
  }

  burst() {
    for (let i = 0; i < 16; i++) {
      const s = new THREE.Mesh(this.sparkGeo, this.sparkMat);
      const from = new THREE.Vector3(1.05 + (Math.random() - 0.5) * 0.6, -1.3 + Math.random() * 0.6, 0.1);
      const to = new THREE.Vector3(i % 2 ? 2.6 : -2.6, 2.6 + Math.random() * 0.6, (Math.random() - 0.5));
      s.position.copy(from);
      this.root.add(s);
      this.sparks.push({ mesh: s, t: -i * 0.03, from, to });
    }
  }

  ambientSpark() {
    if (this.tbillStack.children.length === 0 || Math.random() > 0.04) return;
    const s = new THREE.Mesh(this.sparkGeo, this.sparkMat);
    const top = 0.165 * this.tbillStack.children.length - 1.45;
    const from = new THREE.Vector3(1.05 + (Math.random() - 0.5) * 0.7, top, 0.1 + (Math.random() - 0.5) * 0.3);
    const to = new THREE.Vector3(0.2 * (Math.random() - 0.5), 0.6, 0.3);
    s.position.copy(from);
    s.scale.setScalar(0.7);
    this.root.add(s);
    this.sparks.push({ mesh: s, t: 0, from, to });
  }

  frame(dt: number, t: number) {
    if (!this.dragging) {
      this.vel *= Math.pow(0.12, dt);
      this.hero.rotation.y += (this.spin + this.vel) * dt;
    }
    this.hero.position.y = 1.15 + Math.sin(t * 1.3) * 0.08;
    this.root.rotation.y += ((this.tilt.x * 0.18) - this.root.rotation.y) * Math.min(1, dt * 3);
    this.root.rotation.x += ((this.tilt.y * 0.06) - this.root.rotation.x) * Math.min(1, dt * 3);

    for (const f of [...this.falling]) {
      f.v += 14 * dt;
      f.mesh.position.y -= f.v * dt;
      f.mesh.rotation.x += f.spin * dt;
      if (f.mesh.position.y <= f.target) {
        this.root.remove(f.mesh);
        this.falling.splice(this.falling.indexOf(f), 1);
        this.shown.usdc = -1; // force a restack
      }
    }
    for (const l of [...this.lifting]) {
      l.t += dt;
      l.mesh.position.y += dt * 2.2;
      l.mesh.position.x -= dt * 0.8;
      l.mesh.rotation.z += dt * 3;
      (l.mesh.material as THREE.MeshStandardMaterial[]).forEach((m) => (m.opacity = Math.max(0, 1 - l.t * 1.4)));
      if (l.t > 0.75) { this.root.remove(l.mesh); this.lifting.splice(this.lifting.indexOf(l), 1); }
    }
    this.ambientSpark();
    for (const s of [...this.sparks]) {
      s.t += dt * 0.9;
      if (s.t < 0) continue;
      const k = Math.min(s.t, 1);
      const e = 1 - Math.pow(1 - k, 3);
      s.mesh.position.lerpVectors(s.from, s.to, e);
      s.mesh.position.y += Math.sin(k * Math.PI) * 0.5;
      s.mesh.scale.setScalar(Math.max(0.01, (1 - k) * 1.2));
      if (s.t >= 1) { this.root.remove(s.mesh); this.sparks.splice(this.sparks.indexOf(s), 1); }
    }
    this.renderer.render(this.scene, this.camera);
  }
}

// ---------------------------------------------------------------- boot

let scene: Scene | null = null;
try {
  scene = new Scene($("#stage"));
} catch (e) {
  console.warn("WebGL unavailable; the panel still runs", e);
}

// fonts change the coin face; redraw once they're in
drawFace();
document.fonts?.ready.then(drawFace);

// start the example with a community that has minted some dollars
sim.usdc = 6_000n * USD;
sim.supply = 6_000n * USD;
rebalance();

let prev = performance.now();
function loop(now: number) {
  const dt = Math.min((now - prev) / 1000, 0.05);
  prev = now;
  tickSim(now);
  render();
  scene?.frame(dt, now / 1000);
  requestAnimationFrame(loop);
}
requestAnimationFrame(loop);
