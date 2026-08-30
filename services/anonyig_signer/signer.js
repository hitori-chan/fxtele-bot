#!/usr/bin/env node

// Signs Instagram convert requests the way anonyig.com's own frontend does.
//
// anonyig.com validates each /api/convert request against a signature computed
// by client-side JS that anonyig.com lazy-loads and runs in the browser. That
// module is protected by heavy control-flow obfuscation, not just minification,
// so re-deriving the algorithm by reading it is impractical and would go stale
// on every redeploy anyway. Instead, this script drives a jsdom copy of the real
// page and calls the real signing function directly, so it keeps working across
// anonyig.com's redeploys as long as its general bootstrap shape doesn't change.
//
// IMPORTANT: the signing module must be called immediately after its chunk is
// evaluated, in the same unbroken async chain — inserting so much as one extra
// `.then()` boundary (or any delay) between chunk-eval and the call lets some
// deferred callback the bundle schedules run first, which corrupts its
// internal state (a "SY5enL(...) is not a constructor" failure deep in its
// own secret-decoding path). So nothing here is cached *as a live function*
// across requests: only the fetched, static JS text is cached, and every
// request gets its own fresh jsdom window, fresh chunk evaluation, and an
// immediate call, mirroring exactly what a real page load does.
//
// Protocol: reads one JSON object per line from stdin: {"id": <int>, "target_url": <string>}
// and writes one JSON object per line to stdout: {"id": <int>, "ok": true, "payload": {...}}
// or {"id": <int>, "ok": false, "error": <string>}.

const readline = require("readline");
const { JSDOM } = require("jsdom");

const HOME_URL = "https://anonyig.com/";
const BUNDLE_TTL_MS = 6 * 60 * 60 * 1000;

// The webpack runtime bootstrap assigns its internal require function to `i`
// via this exact minified statement; patching it lets us reach into the
// module registry from outside the bundle's closure afterward.
const REQUIRE_ANCHOR = "i.m=n,";

let bundleCache = null;
let bundlePromise = null;

function makeStorage() {
  const map = new Map();
  return {
    getItem: (key) => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => map.set(key, String(value)),
    removeItem: (key) => map.delete(key),
    clear: () => map.clear(),
  };
}

function findLazySignerIds(appJs) {
  // The real definition assigns a generator function; every other occurrence
  // of "subscribeSignedRequestBody" in the bundle is a plain destructured
  // parameter passthrough (`subscribeSignedRequestBody:i}`), so anchoring on
  // the open paren after the colon picks out the one true definition.
  const defIndex = appJs.indexOf("subscribeSignedRequestBody:(");
  if (defIndex === -1) {
    throw new Error("subscribeSignedRequestBody definition not found in app.js");
  }
  const nearby = appJs.slice(defIndex, defIndex + 500);
  const idsMatch = nearby.match(/n\.e\((\d+)\)\.then\(n\.bind\(n,(\d+)\)\)/);
  if (!idsMatch) {
    throw new Error("lazy chunk/module id pattern not found near subscribeSignedRequestBody");
  }
  return { chunkId: Number(idsMatch[1]), moduleId: Number(idsMatch[2]) };
}

function stubBrowserApis(window) {
  window.localStorage = window.localStorage || makeStorage();
  window.sessionStorage = window.sessionStorage || makeStorage();
  // Deliberately not polyfilling window.fetch: the bundle's own best-effort
  // fingerprint probes (e.g. a relative "/get_country_code" call) expect a
  // browser-shaped fetch that quietly resolves relative URLs against the
  // page location. A Node-backed fetch behaves differently enough there
  // (synchronous throws instead of async rejections) that it derails the
  // signer entirely; leaving fetch undefined lets those probes fail the
  // same "fetch is not defined" way the bundle already tolerates.

  const nodeCrypto = require("crypto");
  window.crypto = window.crypto || nodeCrypto.webcrypto;
  if (window.crypto && !window.crypto.subtle && nodeCrypto.webcrypto) {
    window.crypto.subtle = nodeCrypto.webcrypto.subtle;
  }

  window.matchMedia =
    window.matchMedia ||
    (() => ({
      matches: false,
      addListener() {},
      removeListener() {},
      addEventListener() {},
      removeEventListener() {},
    }));

  try {
    Object.defineProperty(window.navigator, "hardwareConcurrency", { value: 8, configurable: true });
  } catch {
    // Non-configurable on some jsdom versions; the bundle tolerates its absence.
  }

  // Fingerprinting probes call into canvas/WebGL; jsdom doesn't implement
  // either, but the bundle only uses these results as best-effort entropy.
  window.HTMLCanvasElement.prototype.getContext = function (type) {
    if (type === "webgl" || type === "experimental-webgl" || type === "webgl2") {
      return { getParameter: () => "stub", getExtension: () => null, getSupportedExtensions: () => [] };
    }
    return {
      fillRect() {},
      getImageData: () => ({ data: new Uint8ClampedArray(4) }),
      measureText: () => ({ width: 0 }),
      fillText() {},
      createLinearGradient: () => ({ addColorStop() {} }),
    };
  };
  window.HTMLCanvasElement.prototype.toDataURL = () => "data:image/png;base64,stub";

  window.onerror = () => {};
  window.addEventListener("error", (e) => e.preventDefault());
  window.addEventListener("unhandledrejection", (e) => e.preventDefault());
}

async function fetchBundle() {
  const homeResp = await fetch(HOME_URL);
  const homeHtml = await homeResp.text();
  const appJsMatch = homeHtml.match(/src="(\/js\/app\.js\?id=[a-f0-9]+)"/);
  if (!appJsMatch) throw new Error("app.js URL not found on anonyig homepage");
  const appJsUrl = "https://anonyig.com" + appJsMatch[1];

  const appJsResp = await fetch(appJsUrl);
  const appJs = await appJsResp.text();
  if (!appJs.includes(REQUIRE_ANCHOR)) throw new Error("webpack require anchor not found in app.js");

  const { chunkId, moduleId } = findLazySignerIds(appJs);
  const patchedAppJs = appJs.replace(REQUIRE_ANCHOR, REQUIRE_ANCHOR + "self.__require=i;");

  return { patchedAppJs, chunkId, moduleId, fetchedAt: Date.now() };
}

async function getBundle() {
  if (bundleCache && Date.now() - bundleCache.fetchedAt < BUNDLE_TTL_MS) {
    return bundleCache;
  }
  if (!bundlePromise) {
    bundlePromise = fetchBundle()
      .then((bundle) => {
        bundleCache = bundle;
        return bundle;
      })
      .finally(() => {
        bundlePromise = null;
      });
  }
  return bundlePromise;
}

async function signWithBundle(bundle, targetUrl) {
  const dom = new JSDOM('<!DOCTYPE html><html><body><div id="app"></div></body></html>', {
    url: HOME_URL,
    runScripts: "dangerously",
    resources: "usable",
    pretendToBeVisual: true,
  });
  const { window } = dom;

  try {
    stubBrowserApis(window);

    try {
      window.eval(bundle.patchedAppJs);
    } catch {
      // The bundle also tries to mount its Vue app against real DOM elements
      // this shell doesn't have; the webpack runtime is already installed by
      // the time that happens, so the failure is harmless here.
    }

    if (typeof window.__require !== "function") throw new Error("webpack require was not exposed");

    // From here to the signFn() call must stay in one unbroken async chain —
    // see the top-of-file note on why no boundary can be inserted here.
    await window.__require.e(bundle.chunkId);
    const chunkModule = window.__require(bundle.moduleId);
    let signFn = chunkModule.default || chunkModule;
    if (signFn && typeof signFn.then === "function") signFn = await signFn;
    if (typeof signFn !== "function" && signFn && typeof signFn.default === "function") {
      signFn = signFn.default;
    }
    if (typeof signFn !== "function") throw new Error("resolved signer export is not callable");

    return await signFn({ target_url: targetUrl });
  } finally {
    window.close();
  }
}

async function handleRequest({ id, target_url: targetUrl }) {
  try {
    const bundle = await getBundle();
    const payload = await signWithBundle(bundle, targetUrl);
    return { id, ok: true, payload };
  } catch (firstError) {
    // The cached bundle can go stale after an anonyig.com redeploy; force one
    // fresh refetch and retry once before giving up.
    bundleCache = null;
    try {
      const bundle = await getBundle();
      const payload = await signWithBundle(bundle, targetUrl);
      return { id, ok: true, payload };
    } catch (secondError) {
      return { id, ok: false, error: String((secondError && secondError.message) || secondError) };
    }
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });

let pendingRequests = 0;
let stdinEnded = false;

function exitOnceIdle() {
  if (stdinEnded && pendingRequests === 0) process.exit(0);
}

rl.on("line", (rawLine) => {
  const line = rawLine.trim();
  if (!line) return;

  let request;
  try {
    request = JSON.parse(line);
  } catch {
    process.stdout.write(`${JSON.stringify({ ok: false, error: "invalid JSON request" })}\n`);
    return;
  }

  pendingRequests += 1;
  handleRequest(request)
    .then((response) => {
      process.stdout.write(`${JSON.stringify(response)}\n`);
    })
    .finally(() => {
      pendingRequests -= 1;
      exitOnceIdle();
    });
});

process.stdin.on("end", () => {
  stdinEnded = true;
  exitOnceIdle();
});
