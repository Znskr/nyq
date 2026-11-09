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

/* Install box: platform tabs switch the command; the copy button copies it. */
document.querySelectorAll<HTMLElement>("[data-install]").forEach((root) => {
  const tabs = [...root.querySelectorAll<HTMLButtonElement>("[data-os-tab]")];
  const panel = root.querySelector<HTMLElement>("[role=tabpanel]")!;
  const text = root.querySelector<HTMLElement>("[data-cmd-text]")!;
  const button = root.querySelector<HTMLButtonElement>("[data-copy]")!;
  const label = button.querySelector<HTMLElement>("[data-copy-label]")!;
  const status = root.querySelector<HTMLElement>("[data-copy-status]")!;
  let note = status.textContent ?? "";
  let timer: number | undefined;

  const select = (tab: HTMLButtonElement, focus = false) => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
    });
    panel.setAttribute("aria-labelledby", tab.id);
    text.textContent = tab.dataset.cmd!;
    button.dataset.copy = tab.dataset.cmd!;
    note = tab.dataset.note ?? "";
    status.textContent = note;
    if (focus) tab.focus();
  };
  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (e) => {
      const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (!step) return;
      e.preventDefault();
      select(tabs[(i + step + tabs.length) % tabs.length], true);
    });
  });
  // Start on the visitor's platform.
  const ua = navigator.userAgent;
  const guess = /Windows/.test(ua) ? "Windows" : /Mac/.test(ua) ? "macOS" : /Linux|X11/.test(ua) ? "Linux" : null;
  const start = tabs.find((t) => t.textContent?.trim() === guess);
  if (start) select(start);

  button.addEventListener("click", async () => {
    window.clearTimeout(timer);
    try {
      await navigator.clipboard.writeText(button.dataset.copy!);
      button.dataset.state = "copied";
      label.textContent = "copied";
      status.textContent = note;
      timer = window.setTimeout(() => {
        delete button.dataset.state;
        label.textContent = "copy";
      }, 2000);
    } catch {
      // Clipboard blocked (insecure origin or denied): select the text instead.
      window.getSelection()?.selectAllChildren(text);
      status.textContent = "Copy failed. The command is selected, so press Cmd+C or Ctrl+C to copy it.";
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
