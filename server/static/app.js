// Dashboard behaviour, no build step: theme, menu, toasts, confirmations, copying, passkeys,
// cookie files, chart tooltips, the Instant View frame's theme, and live request logs. htmx does
// page changes, forms and refreshing numbers.
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

  function applyTheme(theme) {
    if (theme === "light" || theme === "dark") root.dataset.theme = theme;
    else delete root.dataset.theme;
    themeFrames();
  }

  // The playground's Instant View frame can't see the page's theme, so its <html> gets it.
  const systemDark = matchMedia("(prefers-color-scheme: dark)");
  function themeFrames() {
    const dark = root.dataset.theme ? root.dataset.theme === "dark" : systemDark.matches;
    const theme = dark ? "dark" : "light";
    for (const frame of document.querySelectorAll("iframe[data-iv]")) {
      if (frame.dataset.theme === theme) continue;
      frame.dataset.theme = theme;
      frame.srcdoc = frame.srcdoc.replace(/<html\b[^>]*>/i, `<html lang="en" data-theme="${theme}">`);
    }
  }
  systemDark.addEventListener("change", themeFrames);
  applyTheme(store.get("theme"));

  function setTheme(theme) {
    store.set("theme", theme === "system" ? null : theme);
    applyTheme(theme);
  }

  // The switch's radios show the saved choice; without scripts the switch stays hidden.
  function showTheme() {
    const group = document.querySelector("[data-theme-switch]");
    if (!group) return;
    group.hidden = false;
    const theme = root.dataset.theme || "system";
    for (const input of group.querySelectorAll("input[name=theme]")) input.checked = input.value === theme;
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
    element.append(icon(kind === "error" ? "alert" : "check-circle"), span("", message), close);
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
      await navigator.clipboard.writeText(source.dataset.value ?? source.textContent.trim());
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

  // ---- Chart tooltips: each period's numbers, by the pointer ----

  function showTip(slot, y) {
    const plot = slot.closest(".chart-plot");
    if (!plot || !slot.dataset.tip) return;
    let tip = plot.querySelector(".chart-tip");
    if (!tip) {
      tip = document.createElement("div");
      tip.className = "chart-tip";
      tip.setAttribute("aria-hidden", "true");
      plot.append(tip);
    }
    const data = JSON.parse(slot.dataset.tip);
    tip.replaceChildren(span("tip-time", data.t));
    for (const [series, label, value] of data.rows) {
      const line = span("tip-row", "");
      const value_ = document.createElement("b");
      value_.textContent = value;
      line.append(span(series ? `swatch s-${series}` : "swatch", ""), document.createTextNode(label), value_);
      tip.append(line);
    }
    if (!data.rows.length) tip.append(span("tip-row", "Nothing"));
    tip.hidden = false;
    const area = plot.getBoundingClientRect();
    const box = slot.getBoundingClientRect();
    const half = tip.offsetWidth / 2;
    const x = Math.min(Math.max(box.left + box.width / 2 - area.left, half), area.width - half);
    tip.style.left = `${x}px`;
    tip.style.top = `${Math.max(y - area.top, 0)}px`;
    for (const other of plot.querySelectorAll(".slot.on")) other.classList.remove("on");
    slot.classList.add("on");
  }

  function hideTip(plot) {
    const tip = plot?.querySelector(".chart-tip");
    if (tip) tip.hidden = true;
    for (const slot of plot?.querySelectorAll(".slot.on") ?? []) slot.classList.remove("on");
  }

  // ---- The request log: new requests added live, on the Requests page ----

  const STATES = { live: "Live", paused: "Paused", reconnecting: "Reconnecting…", connecting: "Connecting…" };
  const OUTCOMES = {
    hit: ["ok", "cache hit", "telegram"],
    extracted: ["ok", "extracted", "telegram"],
    redirect: ["idle", "redirected", "redirect"],
    busy: ["warn", "no budget", "telegram"],
    rate_limited: ["warn", "rate limited", "redirect"],
  };

  // The same row the server draws (templates/dashboard/request.html).
  function requestRow(data) {
    const [failed, stage] = data.outcome.split(":");
    const [level, label, kind] = stage
      ? ["bad", stage.replaceAll("_", " ") + (failed === "failed_cached" ? ", cached" : ""), "failed"]
      : (OUTCOMES[data.outcome] ?? ["idle", data.outcome, "telegram"]);
    const row = document.createElement("li");
    row.dataset.kind = kind;
    row.dataset.site = data.source;
    const site = data.source === "-" ? "none" : data.source;
    const mark = span(`site-mark ${site}`, "");
    mark.append(icon(site === "none" ? "globe" : site));
    const what = span("what", "");
    const type = span("type", data.page_type === "-" ? "refused" : data.page_type.replaceAll("_", " "));
    if (data.by === "discord") type.append(span("by", "Discord"));
    what.append(type);
    if (data.link) {
      const link = document.createElement("a");
      link.href = `/playground?url=${encodeURIComponent(data.link)}`;
      link.title = "Open in the playground";
      link.textContent = data.link;
      what.append(link);
    }
    const time = new Date(data.at * 1000).toLocaleTimeString("en-GB", { hour12: false });
    const ms = data.ms >= 1000 ? `${(data.ms / 1000).toFixed(1)} s` : `${data.ms} ms`;
    row.append(span("t", time), mark, what, span(`badge ${level}`, label), span("ms", ms));
    return row;
  }

  const live = {
    source: null,
    paused: false,
    held: [],
    search: "",

    // Connects on pages with a request log, and disconnects on the others.
    sync() {
      const feed = document.querySelector("[data-feed]");
      if (!feed) {
        this.source?.close();
        this.source = null;
        return;
      }
      this.search = document.querySelector("[data-feed-search]")?.value.trim().toLowerCase() ?? "";
      document.querySelector("[data-feed-pause]")?.removeAttribute("hidden");
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
      source.addEventListener("request", (event) => this.request(JSON.parse(event.data)));
    },

    state(name) {
      const element = document.querySelector("[data-live-state]");
      if (!element) return;
      element.dataset.state = name;
      element.textContent = STATES[name];
    },

    request(data) {
      if (this.paused) {
        this.held = [...this.held, data].slice(-500);
        return;
      }
      this.add(data, true);
    },

    // Whether the log's filters (site and kind, as the server applied them) let a row in.
    wants(feed, row) {
      const { site, kind } = feed.dataset;
      if (site && site !== row.dataset.site) return false;
      if (kind === "telegram") return row.dataset.kind !== "redirect";
      return !kind || kind === row.dataset.kind;
    },

    add(data, fresh) {
      const feed = document.querySelector("[data-feed]");
      if (!feed) return;
      const row = requestRow(data);
      if (!this.wants(feed, row)) return;
      feed.querySelector(".empty")?.remove();
      if (fresh) row.classList.add("new");
      row.hidden = !this.matches(row);
      feed.prepend(row);
      const keep = Number(feed.dataset.keep) || 60;
      while (feed.children.length > keep) feed.lastElementChild.remove();
    },

    matches(row) {
      return !this.search || row.textContent.toLowerCase().includes(this.search);
    },

    refilter() {
      for (const row of document.querySelectorAll("[data-feed] li[data-kind]")) row.hidden = !this.matches(row);
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

  // The playground's Telegram: messages say the local time.
  function phone() {
    const time = new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
    for (const element of document.querySelectorAll("[data-now]")) element.textContent = time;
  }

  // The server shows times in this browser's time zone, which it learns from this cookie.
  function keepZone() {
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (!zone || document.cookie.split("; ").includes(`tz=${encodeURIComponent(zone)}`)) return;
    const secure = location.protocol === "https:" ? "; secure" : "";
    document.cookie = `tz=${encodeURIComponent(zone)}; path=/; max-age=31536000; samesite=strict${secure}`;
  }

  // The overview's sites: a click on a site's row shows its kinds of post, and the sites left open
  // stay open through refreshes and visits.
  const openSites = () => new Set((store.get("open-sites") || "").split(",").filter(Boolean));
  function disclose(button, open) {
    button.setAttribute("aria-expanded", String(open));
    const kinds = document.getElementById(button.getAttribute("aria-controls"));
    if (kinds) kinds.hidden = !open;
  }
  function showSites() {
    const open = openSites();
    for (const button of document.querySelectorAll("button.disclose[data-site]")) disclose(button, open.has(button.dataset.site));
  }
  document.addEventListener("click", (event) => {
    const row = event.target.closest?.("tr.group");
    const button = row?.querySelector("button.disclose[data-site]");
    if (!button || (event.target.closest("a") && !event.target.closest("button"))) return;
    const open = button.getAttribute("aria-expanded") !== "true";
    disclose(button, open);
    const sites = openSites();
    if (open) sites.add(button.dataset.site);
    else sites.delete(button.dataset.site);
    store.set("open-sites", sites.size ? [...sites].join(",") : null);
  });

  // ---- Wiring ----

  function ready() {
    keepZone();
    showSites();
    showTheme();
    themeFrames();
    phone();
    // Scripts draw richer tooltips than the charts' titles.
    for (const title of document.querySelectorAll(".chart-svg .slot > title")) title.remove();
    for (const element of document.querySelectorAll("[data-copy], [data-load-into], [data-dropzone]")) element.hidden = false;
    const box = document.getElementById("toasts");
    for (const notice of document.querySelectorAll("#notices > .toast")) box?.append(notice);
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

  // After going to another page, focus starts at its content (unless a field asked for it), as
  // it would after a full load, rather than on the link that's gone.
  document.addEventListener("htmx:afterSettle", (event) => {
    if (!event.detail.boosted || event.detail.requestConfig?.target !== document.body) return;
    const focused = document.activeElement;
    if (!focused || focused === document.body) document.getElementById("main")?.focus({ preventScroll: true });
  });

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
  // An error that isn't a page (a server error, a form the server couldn't read) is plain text;
  // show it without losing the page.
  document.addEventListener("htmx:beforeSwap", (event) => {
    const xhr = event.detail.xhr;
    if (xhr.status >= 400 && !(xhr.getResponseHeader("Content-Type") || "").includes("text/html")) {
      event.detail.shouldSwap = false;
      toast(xhr.responseText.trim() || "Something went wrong; see the server log.", "error");
    }
  });

  // A refused form puts back what was typed in its `data-keep` fields, which the server doesn't
  // send back (pasted cookies).
  const kept = new WeakMap();
  document.addEventListener("htmx:beforeRequest", (event) => {
    const fields = [...(event.detail.elt.querySelectorAll?.("[data-keep]") ?? [])];
    if (fields.length) kept.set(event.detail.elt, fields.map((field) => [field.id, field.value]));
  });
  document.addEventListener("htmx:afterSettle", (event) => {
    const form = event.detail.requestConfig?.elt;
    const values = form && kept.get(form);
    if (!values) return;
    kept.delete(form);
    if (event.detail.xhr?.status < 400) return;
    for (const [id, value] of values) {
      const field = document.getElementById(id);
      if (!field || field.value) continue;
      field.value = value;
      field.closest("details")?.setAttribute("open", "");
      field.focus();
    }
  });

  // A link brought from elsewhere in the dashboard is tried at once (`?run=1`), once.
  const autorun = () => {
    const form = document.querySelector("form[data-autorun]");
    if (!form) return;
    delete form.dataset.autorun;
    history.replaceState(history.state, "", location.pathname + location.search.replace(/[?&]run=1/, "").replace(/^&/, "?"));
    form.requestSubmit();
  };
  document.addEventListener("DOMContentLoaded", autorun);
  document.addEventListener("htmx:load", autorun);

  // An issue linked to from the overview opens.
  const openTarget = () => {
    const target = location.hash && document.getElementById(location.hash.slice(1));
    if (target instanceof HTMLDetailsElement) target.open = true;
  };
  document.addEventListener("DOMContentLoaded", openTarget);
  document.addEventListener("htmx:load", openTarget);
  window.addEventListener("hashchange", openTarget);

  // A code field sends its form once all six digits are in, as authenticator prompts do.
  document.addEventListener("input", (event) => {
    const field = event.target.closest?.("[data-autosubmit]");
    const form = field?.form;
    if (!form || form.dataset.sent || field.value.replace(/\D/g, "").length !== 6 || !form.checkValidity()) return;
    form.dataset.sent = "true";
    form.requestSubmit();
  });

  // A second "again" field says at once when it doesn't match the first.
  document.addEventListener("input", () => {
    for (const again of document.querySelectorAll("[data-same-as]")) {
      const first = document.getElementById(again.dataset.sameAs);
      const differs = first && again.value && again.value !== first.value;
      again.setCustomValidity(differs ? "The two passwords don't match." : "");
    }
  });

  // The button that sent a form, for a question of its own (`data-confirm`) rather than the form's.
  let submitter = null;
  document.addEventListener("submit", (event) => { submitter = event.submitter; }, true);

  // Confirmations in a dialog rather than the browser's own box.
  document.addEventListener("htmx:confirm", (event) => {
    const own = submitter?.form === event.detail.elt ? submitter.dataset.confirm : undefined;
    const question = own ?? event.detail.question;
    const dialog = document.getElementById("confirm");
    if (!question || !dialog || typeof dialog.showModal !== "function") return;
    event.preventDefault();
    dialog.querySelector("[data-confirm-text]").textContent = question;
    // The form's own button names the action, or the one outside it that submits it.
    const elt = event.detail.elt;
    const button = own
      ? submitter
      : (elt.querySelector?.("button") ?? (elt.id && document.querySelector(`button[form="${elt.id}"]`)));
    const action = button?.textContent.trim();
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

  function openMenu(open) {
    const sidebar = document.getElementById("sidebar");
    if (!sidebar) return;
    sidebar.dataset.open = String(open);
    document.querySelector("[data-nav-toggle]")?.setAttribute("aria-expanded", String(open));
  }

  document.addEventListener("click", (event) => {
    // An open status menu closes when anything else is clicked.
    for (const menu of document.querySelectorAll("details.menu[open]")) {
      if (!menu.contains(event.target)) menu.open = false;
    }
    const target = event.target.closest(
      "[data-nav-toggle], [data-nav-close], [data-dismiss], [data-copy], [data-feed-pause], [data-passkey-login]",
    );
    if (!target) return;
    if (target.matches("[data-nav-toggle]")) openMenu(document.getElementById("sidebar")?.dataset.open !== "true");
    else if (target.matches("[data-nav-close]")) openMenu(false);
    else if (target.matches("[data-dismiss]")) dismiss(target.closest(".toast"));
    else if (target.matches("[data-copy]")) {
      event.preventDefault();
      copy(target);
    } else if (target.matches("[data-feed-pause]")) live.togglePause();
    else if (target.matches("[data-passkey-login]")) busy(target, passkeyLogin);
  });

  document.addEventListener("pointerover", (event) => {
    const slot = event.target.closest?.(".chart-svg .slot");
    if (slot) showTip(slot, event.clientY);
  });
  document.addEventListener("pointermove", (event) => {
    const slot = event.target.closest?.(".chart-svg .slot");
    if (slot) showTip(slot, event.clientY);
  });
  document.addEventListener("pointerout", (event) => {
    const svg = event.target.closest?.(".chart-svg");
    if (svg && !svg.contains(event.relatedTarget)) hideTip(svg.closest(".chart-plot"));
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
    if (event.target.matches("[data-theme-switch] input")) {
      setTheme(event.target.value);
      return;
    }
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

  // Escape closes the menu on small screens and the status menu, as it does dialogs.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    openMenu(false);
    for (const menu of document.querySelectorAll("details.menu[open]")) menu.open = false;
    // Like the phone's back button, it leaves Instant View for the chat.
    const article = document.getElementById("iv-open");
    if (article?.checked) article.checked = false;
  });
})();
