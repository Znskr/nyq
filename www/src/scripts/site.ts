/* Theme toggle: the head script already applied the saved or system theme. */
const root = document.documentElement;
const toggles = document.querySelectorAll<HTMLButtonElement>("[data-theme-toggle]");
const system = window.matchMedia("(prefers-color-scheme: light)");
const labelToggles = () => {
  const next = root.dataset.theme === "light" ? "dark" : "light";
  toggles.forEach((b) => b.setAttribute("aria-label", `Switch to ${next} theme`));
};
const setTheme = (theme: string, save: boolean) => {
  root.dataset.theme = theme;
  if (save) {
    try {
      localStorage.setItem("theme", theme);
    } catch {}
  }
  labelToggles();
};
toggles.forEach((b) =>
  b.addEventListener("click", () => setTheme(root.dataset.theme === "light" ? "dark" : "light", true)),
);
system.addEventListener("change", (e) => {
  let saved: string | null = null;
  try {
    saved = localStorage.getItem("theme");
  } catch {}
  if (!saved) setTheme(e.matches ? "light" : "dark", false);
});
labelToggles();

const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* Scroll reveals: each section rises and fades in once. */
const revealer = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      e.target.classList.add("is-visible");
      revealer.unobserve(e.target);
    }
  },
  { rootMargin: "0px 0px -10% 0px", threshold: 0.1 },
);
document.querySelectorAll(".reveal").forEach((el) => revealer.observe(el));

/* Tagline: each word lights up as it crosses a line 60% down the viewport. */
const words = document.querySelectorAll<HTMLElement>("[data-tagline] .tagline-word");
if (reduced) {
  words.forEach((w) => w.classList.add("is-lit"));
} else if (words.length) {
  let ticking = false;
  const update = () => {
    ticking = false;
    const trigger = window.innerHeight * 0.6;
    words.forEach((w) => {
      // Light once; scrolling back up does not dim the words again.
      if (w.getBoundingClientRect().top < trigger) w.classList.add("is-lit");
    });
  };
  const onScroll = () => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(update);
  };
  // Listen only while the tagline is near the viewport.
  const section = words[0].closest("[data-tagline]")!;
  new IntersectionObserver(
    ([e]) => {
      if (e.isIntersecting) {
        window.addEventListener("scroll", onScroll, { passive: true });
        update();
      } else {
        window.removeEventListener("scroll", onScroll);
        update();
      }
    },
    { rootMargin: "50% 0px 50% 0px" },
  ).observe(section);
}

/* Copy the install command. */
document.querySelectorAll<HTMLButtonElement>("[data-copy]").forEach((button) => {
  const label = button.querySelector<HTMLElement>("[data-copy-label]")!;
  const root = button.closest("[data-install]")!;
  const status = root.querySelector<HTMLElement>("[data-copy-status]")!;
  const note = status.innerHTML;
  let timer: number | undefined;

  button.addEventListener("click", async () => {
    window.clearTimeout(timer);
    try {
      await navigator.clipboard.writeText(button.dataset.copy!);
      button.dataset.state = "copied";
      label.textContent = "Copied";
      status.innerHTML = note;
      status.classList.remove("text-nyq-red");
      timer = window.setTimeout(() => {
        delete button.dataset.state;
        label.textContent = "Copy";
      }, 2000);
    } catch {
      // Clipboard blocked (insecure origin or denied): select the text instead.
      const code = root.querySelector("code");
      if (code) window.getSelection()?.selectAllChildren(code);
      status.textContent = "Copy failed. The command is selected, so press Cmd+C or Ctrl+C to copy it.";
      status.classList.add("text-nyq-red");
    }
  });
});

/* Mobile menu. */
const toggle = document.querySelector<HTMLButtonElement>("[data-menu-toggle]");
const menu = document.querySelector<HTMLElement>("[data-menu]");
if (toggle && menu) {
  const setOpen = (open: boolean) => {
    toggle.setAttribute("aria-expanded", String(open));
    toggle.setAttribute("aria-label", open ? "Close menu" : "Open menu");
    menu.toggleAttribute("data-open", open);
    document.body.style.overflow = open ? "hidden" : "";
  };
  toggle.addEventListener("click", () => setOpen(toggle.getAttribute("aria-expanded") !== "true"));
  menu.querySelectorAll("[data-menu-link]").forEach((a) => a.addEventListener("click", () => setOpen(false)));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && menu.hasAttribute("data-open")) {
      setOpen(false);
      toggle.focus();
    }
  });
  window.matchMedia("(min-width: 768px)").addEventListener("change", (e) => e.matches && setOpen(false));
}

/* Mark the nav link of the section in view as current. */
const spies = document.querySelectorAll<HTMLAnchorElement>("[data-spy]");
if (spies.length) {
  const byId = new Map([...spies].map((a) => [a.dataset.spy!, a]));
  const visible = new Set<string>();
  const spy = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) visible.add(e.target.id);
        else visible.delete(e.target.id);
      }
      const current = [...byId.keys()].find((id) => visible.has(id));
      byId.forEach((a, id) => a.setAttribute("aria-current", String(id === current)));
    },
    { rootMargin: "-40% 0px -55% 0px" },
  );
  byId.forEach((_, id) => {
    const section = document.getElementById(id);
    if (section) spy.observe(section);
  });
}
