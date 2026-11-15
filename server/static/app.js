// Dashboard behaviour: passkeys, the overview chart and live updates. No build step.
"use strict";

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

function showError(error) {
  const box = document.querySelector("[data-passkey-error]");
  if (!box) return;
  box.textContent = error.name === "NotAllowedError" ? "Cancelled, or no matching passkey." : error.message;
  box.hidden = false;
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

const COLORS = { hit: "#2a9d8f", extracted: "#457b9d", redirect: "#adb5bd", busy: "#e9c46a", rate_limited: "#f4a261" };

async function drawChart(element) {
  const data = await fetch(element.dataset.series).then((r) => r.json());
  if (!data.timestamps.length) {
    element.textContent = "No requests yet.";
    return;
  }
  const names = Object.keys(data.series);
  const series = [{}].concat(
    names.map((name) => ({
      label: name,
      stroke: COLORS[name] || (name.startsWith("failed") ? "#e63946" : "#6c757d"),
      width: 1.5,
    })),
  );
  const width = () => Math.max(320, element.clientWidth);
  const plot = new uPlot(
    { width: width(), height: 240, series, axes: [{}, { label: "requests / 5 min" }] },
    [data.timestamps].concat(names.map((n) => data.series[n])),
    element,
  );
  addEventListener("resize", () => plot.setSize({ width: width(), height: 240 }));
}

function liveUpdates() {
  const cards = document.querySelector("[data-live]");
  const feed = document.querySelector("[data-feed]");
  if (!cards && !feed) return;
  const events = new EventSource("/events");
  events.addEventListener("snapshot", (event) => {
    const data = JSON.parse(event.data);
    for (const [key, value] of Object.entries(data)) {
      const field = document.querySelector(`[data-field="${key}"]`);
      if (field) field.textContent = value;
    }
  });
  events.addEventListener("request", (event) => {
    if (!feed) return;
    const data = JSON.parse(event.data);
    const item = document.createElement("li");
    item.textContent = `${new Date().toLocaleTimeString()} · ${data.outcome} · ${data.page_type} · ${data.ms} ms`;
    if (feed.firstElementChild?.classList.contains("muted")) feed.firstElementChild.remove();
    feed.prepend(item);
    while (feed.children.length > 20) feed.lastElementChild.remove();
  });
}

document.addEventListener("DOMContentLoaded", () => {
  document.querySelector("[data-passkey-login]")?.addEventListener("click", () => passkeyLogin().catch(showError));
  const register = document.querySelector("[data-passkey-register]");
  register?.addEventListener("submit", (event) => {
    event.preventDefault();
    passkeyRegister(register).catch(showError);
  });
  const chart = document.getElementById("chart");
  if (chart && window.uPlot) drawChart(chart).catch((e) => (chart.textContent = e.message));
  liveUpdates();
});
