// Dashboard behaviour, no build step: theme, menu, toasts, confirmations, copying, passkeys,
// cookie files, and the overview's live numbers and request feed. htmx does page changes and forms.
// Loaded in <head> without `defer`, so the theme is set before the first paint; the rest waits
// for the page. Handlers sit on `document`, so they keep working when htmx swaps the page.
"use strict";

(() => {
  const root = document.documentElement;
  root.classList.add("js");

  const store = {
    get(key) {
      try {
        return localStorage.getItem(key);
      } catch {
        return null;
      }
    },
    set(key, value) {
      try {
        if (value === null) localStorage.removeItem(key);
        else localStorage.setItem(key, value);
      } catch {
        // Private browsing: the choice lasts until the page closes.
      }
    },
  };

  // ---- Theme: system, light or dark, remembered in this browser ----

  const THEMES = ["system", "light", "dark"];
  const THEME_ICONS = { system: "system", light: "sun", dark: "moon" };

  function applyTheme(theme) {
    if (theme === "light" || theme === "dark") root.dataset.theme = theme;
    else delete root.dataset.theme;
  }
  applyTheme(store.get("theme"));

  const currentTheme = () => root.dataset.theme || "system";

  function cycleTheme() {
    const next = THEMES[(THEMES.indexOf(currentTheme()) + 1) % THEMES.length];
    store.set("theme", next === "system" ? null : next);
    applyTheme(next);
    showTheme();
  }

  function showTheme() {
    const button = document.querySelector("[data-theme-toggle]");
    if (!button) return;
    button.hidden = false;
    const theme = currentTheme();
    button.querySelector("[data-theme-label]").textContent = `Theme: ${theme}`;
    const use = button.querySelector("use");
    use?.setAttribute("href", use.getAttribute("href").replace(/#.*$/, `#${THEME_ICONS[theme]}`));
  }

  // ---- Small helpers ----

  function icon(name) {
    const sprite = document.querySelector("use")?.getAttribute("href")?.split("#")[0] ?? "";
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `${sprite}#${name}`);
    svg.append(use);
    return svg;
  }

  function span(className, text) {
    const element = document.createElement("span");
    if (className) element.className = className;
    element.textContent = text;
    return element;
  }

  // ---- Toasts ----

  function dismiss(toast) {
    if (!toast || toast.dataset.leaving) return;
    toast.dataset.leaving = "true";
    toast.classList.add("leaving");
    setTimeout(() => toast.remove(), 250);
  }

  function arm(toast) {
    if (toast.dataset.armed) return;
    toast.dataset.armed = "true";
    toast.querySelector("[data-dismiss]")?.removeAttribute("hidden");
    // Errors stay until dismissed; notices go by themselves, unless the pointer rests on them.
    if (!toast.classList.contains("error")) {
      let timer = setTimeout(() => dismiss(toast), 6000);
      toast.addEventListener("mouseenter", () => clearTimeout(timer));
      toast.addEventListener("mouseleave", () => (timer = setTimeout(() => dismiss(toast), 2500)));
    }
  }

  function toast(message, kind) {
    const box = document.querySelector(".toasts");
    if (!box) return;
    const element = document.createElement("div");
    element.className = kind === "error" ? "toast error" : "toast";
    element.setAttribute("role", kind === "error" ? "alert" : "status");
    const close = document.createElement("button");
    close.type = "button";
    close.className = "toast-close";
    close.setAttribute("aria-label", "Dismiss");
    close.dataset.dismiss = "";
    close.textContent = "×";
    element.append(icon(kind === "error" ? "alert" : "check"), span("", message), close);
    box.append(element);
    arm(element);
  }

  // `?done=` shows its notice once; a reload or a shared link shouldn't show it again.
  function forgetNotice() {
    const url = new URL(location.href);
    if (!url.searchParams.has("done")) return;
    url.searchParams.delete("done");
    history.replaceState(history.state, "", url.pathname + url.search + url.hash);
  }

  // ---- Copying ----

  async function copy(button) {
    const source = document.querySelector(button.dataset.copy);
    const label = [...button.childNodes].find((n) => n.nodeType === Node.TEXT_NODE && n.nodeValue.trim());
    if (!source || !label) return;
    const before = label.nodeValue;
    try {
      await navigator.clipboard.writeText(source.textContent.trim());
      label.nodeValue = "Copied";
    } catch {
      label.nodeValue = "Select and copy it instead";
    }
    setTimeout(() => (label.nodeValue = before), 1600);
  }

  // ---- Passkeys ----

  const b64u = {
    encode(buffer) {
      let text = "";
      for (const byte of new Uint8Array(buffer)) text += String.fromCharCode(byte);
      return btoa(text).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    },
    decode(value) {
      const base64 = value.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (value.length % 4)) % 4);
      return Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    },
  };

  async function postJson(url, body) {
    const response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body ?? {}),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || `${response.status} ${response.statusText}`);
    return data;
  }

  function passkeyError(error) {
    const box = document.querySelector("[data-passkey-error]");
    if (!box) return;
    box.textContent = error.name === "NotAllowedError" ? "Cancelled, or no matching passkey." : error.message;
    box.hidden = false;
  }

  async function busy(button, work) {
    button.setAttribute("aria-busy", "true");
    button.disabled = true;
    try {
      await work();
    } catch (error) {
      passkeyError(error);
    } finally {
      button.removeAttribute("aria-busy");
      button.disabled = false;
    }
  }

  async function passkeyLogin() {
    const start = await postJson("/login/passkey/start");
    const options = start.options.publicKey;
    options.challenge = b64u.decode(options.challenge);
    for (const c of options.allowCredentials ?? []) c.id = b64u.decode(c.id);
    const credential = await navigator.credentials.get({ publicKey: options });
    const r = credential.response;
    await postJson("/login/passkey/finish", {
      id: start.id,
      credential: {
        id: credential.id,
        rawId: b64u.encode(credential.rawId),
        type: credential.type,
        response: {
          authenticatorData: b64u.encode(r.authenticatorData),
          clientDataJSON: b64u.encode(r.clientDataJSON),
          signature: b64u.encode(r.signature),
          userHandle: r.userHandle ? b64u.encode(r.userHandle) : null,
        },
        extensions: credential.getClientExtensionResults(),
      },
    });
    location.href = "/";
  }

  async function passkeyRegister(form) {
    const name = new FormData(form).get("name") || "";
    const start = await postJson("/security/passkeys/start", { name });
    const options = start.options.publicKey;
    options.challenge = b64u.decode(options.challenge);
    options.user.id = b64u.decode(options.user.id);
    for (const c of options.excludeCredentials ?? []) c.id = b64u.decode(c.id);
    const credential = await navigator.credentials.create({ publicKey: options });
    const r = credential.response;
    await postJson("/security/passkeys/finish", {
      id: start.id,
      credential: {
        id: credential.id,
        rawId: b64u.encode(credential.rawId),
        type: credential.type,
        response: {
          attestationObject: b64u.encode(r.attestationObject),
          clientDataJSON: b64u.encode(r.clientDataJSON),
          transports: r.getTransports ? r.getTransports() : undefined,
        },
        extensions: credential.getClientExtensionResults(),
      },
    });
    location.reload();
  }

  // ---- Cookie files: read here and sent like a paste, so they never sit in a field ----

  const COOKIE_FILE_LIMIT = 1 << 20;

  async function sendCookieFile(file, zone) {
    if (file.size > COOKIE_FILE_LIMIT) {
      toast("That file is too big to be a cookie export.", "error");
      return;
    }
    const form = zone.closest("form");
    form.querySelector("textarea[name=cookies]").value = await file.text();
    zone.setAttribute("aria-busy", "true");
    form.requestSubmit();
  }

  // ---- The overview: live numbers and the request feed ----

  const OUTCOMES = {
    hit: ["ok", "from the cache"],
    extracted: ["info", "extracted"],
    busy: ["warn", "busy"],
    rate_limited: ["warn", "rate limited"],
    redirect: ["idle", "redirected"],
  };
  const STATES = { live: "Live", paused: "Paused", reconnecting: "Reconnecting…", connecting: "Connecting…" };
  const FEED_ROWS = 60;

  const live = {
    source: null,
    paused: false,
    held: [],
    filter: "all",
    search: "",

    // Connects on pages that show live data, and disconnects on the others.
    sync() {
      const page = document.querySelector("[data-live], [data-feed]");
      if (!page) {
        this.source?.close();
        this.source = null;
        return;
      }
      for (const element of document.querySelectorAll("[data-feed-pause], [data-feed-filters]")) element.hidden = false;
      this.showPause();
      if (this.source) {
        this.state(this.paused ? "paused" : this.source.readyState === EventSource.OPEN ? "live" : "connecting");
        return;
      }
      const source = new EventSource("/events");
      this.source = source;
      this.state("connecting");
      source.addEventListener("open", () => this.state(this.paused ? "paused" : "live"));
      source.addEventListener("error", () => this.state("reconnecting"));
      source.addEventListener("snapshot", (event) => this.snapshot(JSON.parse(event.data)));
      source.addEventListener("request", (event) => this.request(JSON.parse(event.data)));
    },

    state(name) {
      const element = document.querySelector("[data-live-state]");
      if (!element) return;
      element.dataset.state = name;
      element.textContent = STATES[name];
    },

    snapshot(data) {
      for (const [key, value] of Object.entries(data)) {
        const field = document.querySelector(`[data-field="${key}"]`);
        if (!field || field.textContent === String(value)) continue;
        field.textContent = value;
        field.classList.remove("flash");
        void field.offsetWidth;
        field.classList.add("flash");
      }
    },

    request(data) {
      if (this.paused) {
        this.held.push(data);
        this.held = this.held.slice(-FEED_ROWS);
        return;
      }
      this.add(data, true);
    },

    add(data, fresh) {
      const feed = document.querySelector("[data-feed]");
      if (!feed) return;
      feed.querySelector(".empty")?.remove();
      const failed = data.outcome.startsWith("failed:");
      const [level, label] = failed ? ["bad", data.outcome.replace("failed:", "failed: ").replace("_", " ")] : OUTCOMES[data.outcome] ?? ["idle", data.outcome];
      const row = document.createElement("li");
      row.dataset.kind = data.outcome === "redirect" ? "redirect" : failed ? "failed" : "telegram";
      row.dataset.type = data.page_type;
      const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
      row.append(span("t", time), span(`pill ${level}`, label), span("", data.page_type), span("ms", `${data.ms} ms`));
      if (fresh) row.classList.add("new");
      row.hidden = !this.shows(row);
      feed.prepend(row);
      while (feed.children.length > FEED_ROWS) feed.lastElementChild.remove();
    },

    shows(row) {
      const kind = row.dataset.kind;
      const byKind =
        this.filter === "all" ||
        (this.filter === "telegram" && kind !== "redirect") ||
        (this.filter === "failed" && kind === "failed") ||
        (this.filter === "redirect" && kind === "redirect");
      return byKind && (!this.search || row.dataset.type.includes(this.search));
    },

    refilter() {
      for (const row of document.querySelectorAll("[data-feed] li[data-kind]")) row.hidden = !this.shows(row);
    },

    setFilter(button) {
      this.filter = button.dataset.filter;
      for (const other of button.parentElement.querySelectorAll("[data-filter]")) {
        other.setAttribute("aria-pressed", String(other === button));
      }
      this.refilter();
    },

    togglePause() {
      this.paused = !this.paused;
      if (!this.paused) {
        for (const data of this.held) this.add(data, true);
        this.held = [];
      }
      this.showPause();
      this.state(this.paused ? "paused" : "live");
    },

    showPause() {
      const button = document.querySelector("[data-feed-pause]");
      if (!button) return;
      button.querySelector("span").textContent = this.paused ? "Resume" : "Pause";
      const use = button.querySelector("use");
      use?.setAttribute("href", use.getAttribute("href").replace(/#.*$/, this.paused ? "#play" : "#pause"));
    },
  };

  // ---- Wiring ----

  function ready() {
    showTheme();
    for (const element of document.querySelectorAll("[data-copy], [data-load-into], [data-dropzone]")) element.hidden = false;
    for (const element of document.querySelectorAll("[data-toast], .toast")) arm(element);
    forgetNotice();
    live.sync();
  }

  // One run per swap, however many new elements htmx reports.
  let scheduled = false;
  function schedule() {
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      ready();
    });
  }
  document.addEventListener("DOMContentLoaded", () => {
    if (!window.htmx) ready();
  });
  document.addEventListener("htmx:load", schedule);

  // A thin bar while a page loads; polls and live searches don't show it.
  let inFlight = 0;
  const quiet = (event) => event.detail?.elt?.closest?.("[data-quiet]");
  document.addEventListener("htmx:beforeRequest", (event) => {
    if (quiet(event)) return;
    inFlight += 1;
    root.classList.add("loading");
  });
  document.addEventListener("htmx:afterRequest", (event) => {
    if (quiet(event)) return;
    inFlight = Math.max(0, inFlight - 1);
    if (!inFlight) root.classList.remove("loading");
  });
  // Polls say so: they aren't use, so a tab left open doesn't keep the session from going idle.
  document.addEventListener("htmx:configRequest", (event) => {
    if (event.detail.elt?.hasAttribute?.("data-poll")) event.detail.headers["Homura-Poll"] = "1";
  });
  document.addEventListener("htmx:sendError", () => {
    toast("Couldn't reach the dashboard. Check the connection, then try again.", "error");
  });
  // A server error is plain text; show it without losing the page.
  document.addEventListener("htmx:beforeSwap", (event) => {
    const xhr = event.detail.xhr;
    if (xhr.status >= 500 && !(xhr.getResponseHeader("Content-Type") || "").includes("text/html")) {
      event.detail.shouldSwap = false;
      toast(xhr.responseText.trim() || "Something went wrong; see the server log.", "error");
    }
  });

  // Confirmations in a dialog rather than the browser's own box.
  document.addEventListener("htmx:confirm", (event) => {
    const question = event.detail.question;
    const dialog = document.getElementById("confirm");
    if (!question || !dialog || typeof dialog.showModal !== "function") return;
    event.preventDefault();
    dialog.querySelector("[data-confirm-text]").textContent = question;
    const action = event.detail.elt.querySelector?.("button")?.textContent.trim();
    dialog.querySelector("[data-confirm-ok]").textContent = action || "Continue";
    dialog.returnValue = "";
    dialog.addEventListener(
      "close",
      () => {
        if (dialog.returnValue === "ok") event.detail.issueRequest(true);
      },
      { once: true },
    );
    dialog.showModal();
  });

  document.addEventListener("click", (event) => {
    const target = event.target.closest(
      "[data-nav-toggle], [data-theme-toggle], [data-dismiss], [data-copy], [data-feed-pause], [data-filter], [data-passkey-login]",
    );
    if (!target) return;
    if (target.matches("[data-nav-toggle]")) {
      const sidebar = document.getElementById("sidebar");
      const open = sidebar.dataset.open !== "true";
      sidebar.dataset.open = String(open);
      target.setAttribute("aria-expanded", String(open));
    } else if (target.matches("[data-theme-toggle]")) cycleTheme();
    else if (target.matches("[data-dismiss]")) dismiss(target.closest(".toast"));
    else if (target.matches("[data-copy]")) {
      event.preventDefault();
      copy(target);
    } else if (target.matches("[data-feed-pause]")) live.togglePause();
    else if (target.matches("[data-filter]")) live.setFilter(target);
    else if (target.matches("[data-passkey-login]")) busy(target, passkeyLogin);
  });

  document.addEventListener("submit", (event) => {
    const form = event.target.closest("[data-passkey-register]");
    if (!form) return;
    event.preventDefault();
    busy(form.querySelector("button"), () => passkeyRegister(form));
  });

  document.addEventListener("input", (event) => {
    if (event.target.matches("[data-feed-search]")) {
      live.search = event.target.value.trim().toLowerCase();
      live.refilter();
    }
  });

  document.addEventListener("change", (event) => {
    const cookies = event.target.closest("[data-cookie-file]");
    if (cookies?.files?.[0]) {
      sendCookieFile(cookies.files[0], cookies.closest("[data-dropzone]"));
      cookies.value = "";
      return;
    }
    const input = event.target.closest("[data-load-into]");
    const file = input?.files?.[0];
    if (file) file.text().then((text) => (document.querySelector(input.dataset.loadInto).value = text));
  });

  // Dropping a file on a drop zone.
  for (const type of ["dragenter", "dragover", "dragleave", "drop"]) {
    document.addEventListener(type, (event) => {
      const zone = event.target.closest?.("[data-dropzone]");
      if (!zone) return;
      event.preventDefault();
      zone.classList.toggle("over", type === "dragenter" || type === "dragover");
      const file = type === "drop" && event.dataTransfer?.files?.[0];
      if (file) sendCookieFile(file, zone);
    });
  }

  // Escape closes the menu on small screens, as it does dialogs.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    const sidebar = document.getElementById("sidebar");
    if (sidebar) sidebar.dataset.open = "false";
  });
})();
