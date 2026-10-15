import "./styles.css";

// Highlight the sidebar entry for the section in view.
const links = new Map<string, HTMLAnchorElement>();
document.querySelectorAll<HTMLAnchorElement>(".side a").forEach((a) => links.set(a.hash.slice(1), a));

function mark(id: string) {
  links.forEach((a, k) => a.classList.toggle("on", k === id));
}

const sections = [...document.querySelectorAll<HTMLElement>(".prose section")];
function onScroll() {
  let current = sections[0]?.id;
  for (const s of sections) if (s.getBoundingClientRect().top < 140) current = s.id;
  if (window.innerHeight + window.scrollY >= document.body.scrollHeight - 4) current = sections[sections.length - 1].id;
  mark(current);
}
addEventListener("scroll", onScroll, { passive: true });
onScroll();

const side = document.getElementById("side")!;
document.getElementById("menu")!.addEventListener("click", () => side.classList.toggle("open"));
side.addEventListener("click", (e) => { if ((e.target as HTMLElement).tagName === "A") side.classList.remove("open"); });
