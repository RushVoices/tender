import "./styles.css";

// Each section has a clean address (/docs/quickstart). The sidebar scrolls in
// place, and the address follows whichever section is in view.
const links = new Map<string, HTMLAnchorElement>();
document.querySelectorAll<HTMLAnchorElement>(".side a").forEach((a) => links.set(a.pathname.split("/")[2] ?? "", a));

const sections = [...document.querySelectorAll<HTMLElement>(".prose section")];
let current = "";

function mark(id: string) {
  if (id === current) return;
  current = id;
  links.forEach((a, k) => a.classList.toggle("on", k === id));
  const path = id === sections[0]?.id ? "/docs" : `/docs/${id}`;
  if (location.pathname !== path) history.replaceState(null, "", path);
}

function onScroll() {
  let id = sections[0]?.id;
  for (const s of sections) if (s.getBoundingClientRect().top < 140) id = s.id;
  if (window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 4) id = sections[sections.length - 1].id;
  mark(id);
}

function scrollToSection(id: string, smooth: boolean) {
  const el = document.getElementById(id);
  if (!el) return false;
  el.scrollIntoView({ behavior: smooth ? "smooth" : "auto", block: "start" });
  return true;
}

// Old links (/docs.html#risks) and new ones (/docs/risks) both land on the section.
const wanted = location.pathname.split("/")[2] || location.hash.slice(1);
if (wanted && scrollToSection(wanted, false)) {
  history.replaceState(null, "", `/docs/${wanted}`);
  current = wanted;
  links.forEach((a, k) => a.classList.toggle("on", k === wanted));
} else if (location.pathname !== "/docs") {
  history.replaceState(null, "", "/docs");
}
addEventListener("scroll", onScroll, { passive: true });
onScroll();

const side = document.getElementById("side")!;
document.getElementById("menu")!.addEventListener("click", () => side.classList.toggle("open"));
document.addEventListener("click", (e) => {
  const a = (e.target as Element).closest?.("a");
  if (!a || a.origin !== location.origin || !a.pathname.startsWith("/docs/") || e.metaKey || e.ctrlKey) return;
  const id = a.pathname.split("/")[2];
  if (!scrollToSection(id, true)) return;
  e.preventDefault();
  side.classList.remove("open");
  history.pushState(null, "", `/docs/${id}`);
});
addEventListener("popstate", () => {
  const id = location.pathname.split("/")[2] || sections[0]?.id;
  if (id) scrollToSection(id, false);
});
