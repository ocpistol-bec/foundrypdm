// Reading-font setting checks for the web portal: bundled faces, @font-face rules, the per-browser
// choice in Settings and the <head> script that applies it before first paint.
// Run with: node --test crates/pdm-api/tests/web-ui-fonts.test.cjs
const { test } = require("node:test");
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const STATIC = path.join(__dirname, "../static");
const css = fs.readFileSync(path.join(STATIC, "style.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
const app = fs.readFileSync(path.join(STATIC, "app.js"), "utf8");
const indexHtml = fs.readFileSync(path.join(STATIC, "index.html"), "utf8");

function loadFunctions(names, globals){
  const context = vm.createContext(globals);
  for (const name of names){
    const start = app.indexOf("function " + name + "(");
    assert(start >= 0, "missing function " + name);
    const asyncStart = app.slice(start - 6, start) === "async " ? start - 6 : start;
    const rest = app.slice(start);
    const end = rest.search(/\n(?:async )?function /);
    vm.runInContext(app.slice(asyncStart, end < 0 ? app.length : start + end), context);
  }
  return context;
}

const esc = s => String(s).replace(/[&<>"']/g, c => ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", "\"":"&quot;", "'":"&#39;" })[c]);
const settings = () => loadFunctions(["fontSettings"], {}).fontSettings();
// Declarations of the rules with exactly this selector, as a Map.
function ruleDecls(selector){
  const found = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].filter(m => m[1].trim() === selector);
  assert(found.length > 0, "a rule for " + selector);
  return new Map(found.flatMap(m => m[2].split(";").map(d => d.match(/^\s*([-\w]+)\s*:\s*([\s\S]+?)\s*$/))
    .filter(Boolean).map(d => [d[1], d[2]])));
}
const fontFaces = () => [...css.matchAll(/@font-face\s*\{([^}]*)\}/g)].map(m => new Map(m[1].split(";")
  .map(d => d.match(/^\s*([-\w]+)\s*:\s*([\s\S]+?)\s*$/)).filter(Boolean).map(d => [d[1], d[2]])));

// ── Bundled files ───────────────────────────────────────────────────────
const FONTS_DIR = path.join(STATIC, "fonts");
const FAMILIES = {
  "Atkinson Hyperlegible Next":"atkinson-hyperlegible-next",
  "Atkinson Hyperlegible Mono":"atkinson-hyperlegible-mono",
};

// FONTS.txt records the SHA-256 of each file as published at the source commit it names.
test("each bundled file matches the SHA-256 that FONTS.txt records for it", () => {
  const listed = [...fs.readFileSync(path.join(FONTS_DIR, "FONTS.txt"), "utf8")
    .matchAll(/^([0-9a-f]{64})\s+(\S+\.(?:ttf|txt))\b/gm)].map(m => ({ sha:m[1], file:m[2] }));
  const onDisk = Object.values(FAMILIES).flatMap(dir => fs.readdirSync(path.join(FONTS_DIR, dir)).map(f => dir + "/" + f));
  assert.deepEqual(listed.map(l => l.file).sort(), onDisk.sort(), "FONTS.txt lists exactly the files beside the fonts");
  assert.equal(onDisk.filter(f => /\.(ttf|otf|woff2?)$/.test(f)).length, 4, "upright and italic of each family");
  for (const { sha, file } of listed){
    let bytes = fs.readFileSync(path.join(FONTS_DIR, file));
    // License text is hashed with LF line endings; a Windows checkout may have turned them into CRLF.
    if (file.endsWith(".txt")) bytes = Buffer.from(bytes.toString("latin1").replace(/\r\n/g, "\n"), "latin1");
    assert.equal(crypto.createHash("sha256").update(bytes).digest("hex"), sha, file + " matches its recorded SHA-256");
  }
  const total = onDisk.reduce((n, f) => n + fs.statSync(path.join(FONTS_DIR, f)).size, 0);
  assert(total < 400 * 1024, "all faces together stay under 400 KB (" + total + " bytes)");
});

test("the SIL Open Font License travels beside each family", () => {
  for (const [family, dir] of Object.entries(FAMILIES)){
    const ofl = fs.readFileSync(path.join(FONTS_DIR, dir, "OFL.txt"), "utf8");
    assert.match(ofl, new RegExp("^Copyright .*" + family + " Project Authors"), dir + "/OFL.txt keeps its copyright line");
    assert.match(ofl, /SIL OPEN FONT LICENSE Version 1\.1/, dir + "/OFL.txt is the OFL 1.1 text");
  }
});

// ── @font-face and the font tokens ──────────────────────────────────────
test("@font-face declares an upright and an italic variable face per family, swapped in when loaded", () => {
  const faces = fontFaces();
  const seen = [];
  for (const f of faces){
    const family = (f.get("font-family") || "").replace(/^"|"$/g, "");
    assert(FAMILIES[family], "known family " + family);
    seen.push(family + "/" + f.get("font-style"));
    assert.equal(f.get("font-weight"), "200 800", family + " covers the weight axis");
    assert.equal(f.get("font-display"), "swap", family + " shows fallback text while loading");
    const src = (f.get("src") || "").match(/^url\("fonts\/([^"]+)"\) format\("truetype"\)$/);
    assert(src, family + " src is one local file: " + f.get("src"));
    assert(src[1].startsWith(FAMILIES[family] + "/"), family + " file sits in its own folder");
    assert(fs.existsSync(path.join(FONTS_DIR, src[1])), src[1] + " exists");
    assert.equal(/Italic/.test(src[1]), f.get("font-style") === "italic", src[1] + " matches its font-style");
  }
  assert.deepEqual(seen.sort(), Object.keys(FAMILIES).flatMap(f => [f + "/italic", f + "/normal"]).sort());
  assert.doesNotMatch(css, /@import|https?:\/\//, "no remote fonts or stylesheets: installs can be offline");
});

test("the font tokens default to the system stacks and a choice prepends its bundled face", () => {
  const root = ruleDecls(":root");
  assert.match(root.get("--font-ui"), /^system-ui,.*sans-serif$/);
  assert.match(root.get("--font-mono"), /^ui-monospace,.*monospace$/);
  for (const s of settings()){
    const token = "--" + s.attr.replace(/^data-/, "");
    assert(root.has(token), s.attr + " has a matching token " + token);
    for (const o of s.options.filter(o => o.id !== "system")){
      const v = ruleDecls("html[" + s.attr + "=\"" + o.id + "\"]").get(token);
      assert.equal(v, "\"" + o.name + "\"," + root.get(token), o.name + " falls back to the default stack");
      assert(fontFaces().some(f => f.get("font-family") === "\"" + o.name + "\""), o.name + " has @font-face rules");
    }
  }
  assert.equal(ruleDecls("body").get("font"), "14px/1.5 var(--font-ui)", "body text uses the reading font, same size");
  assert.equal(ruleDecls(".mono").get("font-family"), "var(--font-mono)");
});

test("a bundled face is named only by its @font-face and choice rules, so the default loads no font file", () => {
  const outside = css.replace(/@font-face\s*\{[^}]*\}/g, "").replace(/html\[data-font-[\w-]+="[\w-]+"\]\s*\{[^}]*\}/g, "");
  assert.doesNotMatch(outside, /Atkinson/, "style.css names a bundled face outside its rules");
  assert.doesNotMatch(app, /style=[^>]*Atkinson|font-family/, "app.js shows no sample in a bundled face");
});

// ── Applied before first paint ──────────────────────────────────────────
function headScript(){
  const head = indexHtml.slice(0, indexHtml.indexOf("</head>"));
  const scripts = [...head.matchAll(/<script>([\s\S]*?)<\/script>/g)].map(m => m[1]).filter(s => s.includes("pdm_font_ui"));
  assert.equal(scripts.length, 1, "one inline <head> script reads the font choice");
  assert.match(scripts[0], /pdm_theme/, "the font choice is applied by the same script as the theme");
  return scripts[0];
}
// Returns the attributes set on <html> and the links added to <head>; throws if the script itself fails.
function runHead(stored, opts = {}){
  const attrs = {}, links = [];
  const localStorage = { getItem(k){ if (opts.throws) throw new Error("storage blocked"); return k in stored ? stored[k] : null; } };
  const document = {
    documentElement:{ setAttribute(k, v){ attrs[k] = v; }, getAttribute(k){ return attrs[k]; } },
    createElement(tag){ assert.equal(tag, "link"); return {}; },
    head:{ appendChild(el){ links.push(Object.assign({}, el)); } },
  };
  const context = vm.createContext({ localStorage, document, assert });
  // Let errors through the outer catch (the one that guards storage access), so a broken script fails the test.
  const src = headScript(), at = src.lastIndexOf("} catch (e) {}");
  vm.runInContext(src.slice(0, at) + "} catch (e) { if (!" + Boolean(opts.throws) + ") throw e; }" + src.slice(at + 14), context);
  assert.deepEqual(Object.keys(context).sort(), ["assert", "document", "localStorage"], "the script leaves no globals behind");
  return { attrs, links };
}
// The upright face of a family, as style.css's @font-face names it, as a URL path.
const uprightSrc = family => "/static/fonts/" + fontFaces().find(f => f.get("font-family") === "\"" + family + "\"" &&
  f.get("font-style") === "normal").get("src").match(/url\("fonts\/([^"]+)"\)/)[1];

test("index.html applies the saved fonts in <head>, with the same keys, ids and default as app.js", () => {
  assert(indexHtml.indexOf("pdm_font_ui") < indexHtml.indexOf("<link rel=\"stylesheet\""), "before the stylesheets load");
  for (const s of settings()){
    for (const id of [...s.options.map(o => o.id), "org", "neon", "Atkinson", "", null]){
      const { attrs, links } = runHead({ [s.key]:id });
      const chosen = id !== "system" && s.options.some(o => o.id === id);
      assert.deepEqual(attrs, chosen ? { [s.attr]:id } : {}, s.key + "=" + id);
      // A chosen face is fetched at once (preload), so the first paint can use it instead of swapping later.
      const face = chosen && s.options.find(o => o.id === id).name;
      assert.deepEqual(links, chosen ? [{ rel:"preload", as:"font", type:"font/ttf", crossOrigin:"anonymous", href:uprightSrc(face) }] : [],
        s.key + "=" + id + " preload");
    }
  }
  const both = runHead({ pdm_theme:"light", pdm_font_ui:"atkinson", pdm_font_mono:"atkinson" });
  assert.deepEqual(both.attrs, { "data-theme":"light", "data-font-ui":"atkinson", "data-font-mono":"atkinson" });
  assert.equal(both.links.length, 2);
  assert.deepEqual(runHead({ pdm_font_ui:"atkinson" }, { throws:true }), { attrs:{}, links:[] }, "blocked storage does not stop the page");
  assert.doesNotMatch(indexHtml.match(/<html[^>]*>/)[0], /data-font/, "no font attribute without a saved choice");
});

// ── The setting in Settings ─────────────────────────────────────────────
const FONT_FNS = ["fontSettings", "orgFont", "orgFace", "orgOptionName", "currentFont", "effectiveFont", "applyFonts",
  "applyFont", "fontPickerHTML", "orgFontPickerHTML", "wireFontPicker", "loadOrgFont", "syncFontPicker", "cacheOrgFont", "refreshOrgFont",
  "saveOrgFont"];
function fontCtx(stored = {}, opts = {}){
  const attrs = {}, calls = [], notes = [];
  const ctx = loadFunctions(FONT_FNS, {
    esc,
    isRole: min => min !== "admin" || opts.role === "admin",
    toast: (m, k) => notes.push([m, k]),
    api: async (method, url, body) => {
      calls.push({ method, url, body });
      return opts.api ? opts.api(method, url, body) : { ui:"system", mono:"system" };
    },
    localStorage:{ getItem:k => (k in stored ? stored[k] : null), setItem(k, v){ stored[k] = String(v); }, removeItem(k){ delete stored[k]; } },
    document:{ documentElement:{ setAttribute(k, v){ attrs[k] = v; }, removeAttribute(k){ delete attrs[k]; } } },
  });
  return { ctx, attrs, stored, calls, notes };
}
const ORG_BOTH = JSON.stringify({ ui:"atkinson", mono:"atkinson" });

test("without a face of their own, members get the organization default, in <head> and in app.js alike", () => {
  const values = ["system", "atkinson", "org", "neon", null];
  const orgs = [null, ORG_BOTH, JSON.stringify({ ui:"system", mono:"atkinson" }), JSON.stringify({ ui:"comic" }), "{not json", "\"atkinson\""];
  for (const ui of values) for (const mono of values) for (const org of orgs){
    const stored = {};
    if (ui !== null) stored.pdm_font_ui = ui;
    if (mono !== null) stored.pdm_font_mono = mono;
    if (org !== null) stored.pdm_org_font = org;
    const head = runHead(stored);
    const { ctx, attrs } = fontCtx(Object.assign({}, stored));
    ctx.applyFonts();
    assert.deepEqual(head.attrs, attrs, "head script and app.js agree for " + JSON.stringify(stored));
    assert.equal(head.links.length, Object.keys(head.attrs).length, "a preload for each applied face");
  }
  assert.deepEqual(runHead({ pdm_org_font:ORG_BOTH }).attrs, { "data-font-ui":"atkinson", "data-font-mono":"atkinson" },
    "no choice of their own: the organization default");
  assert.deepEqual(runHead({ pdm_org_font:ORG_BOTH, pdm_font_ui:"system" }).attrs, { "data-font-mono":"atkinson" },
    "an explicit System default wins over the organization default");
  assert.deepEqual(runHead({ pdm_org_font:"{not json" }).attrs, {}, "a broken cache is ignored");
});

test("each font setting follows the organization default unless the member picked a face", () => {
  const { ctx } = fontCtx({ pdm_font_ui:"atkinson", pdm_font_mono:"comic", pdm_org_font:JSON.stringify({ ui:"system", mono:"atkinson" }) });
  const [ui, mono] = ctx.fontSettings();
  assert.equal(ctx.currentFont(ui), "atkinson");
  assert.equal(ctx.currentFont(mono), "org", "an unknown saved value follows the organization");
  assert.equal(ctx.effectiveFont(mono), "atkinson");
  assert.equal(fontCtx().ctx.currentFont(ui), "org", "nothing saved: Organization default");
  assert.equal(fontCtx().ctx.effectiveFont(ui), "system", "no organization default known: the system font");
  assert.deepEqual([...ctx.fontSettings().map(s => s.key)], ["pdm_font_ui", "pdm_font_mono"]);
});

test("choosing a font applies it at once and remembers it; Organization default hands the choice back", () => {
  const { ctx, attrs, stored } = fontCtx({ pdm_org_font:JSON.stringify({ ui:"atkinson", mono:"system" }) });
  const [ui, mono] = ctx.fontSettings();
  ctx.applyFont(ui, "system");
  ctx.applyFont(mono, "atkinson");
  assert.deepEqual(attrs, { "data-font-mono":"atkinson" }, "own choices win over the organization default");
  assert.equal(stored.pdm_font_ui, "system");
  assert.equal(stored.pdm_font_mono, "atkinson");
  ctx.applyFont(ui, "org");
  ctx.applyFont(mono, "nope");
  assert.deepEqual(attrs, { "data-font-ui":"atkinson" }, "back on the organization default");
  assert(!("pdm_font_ui" in stored) && !("pdm_font_mono" in stored), "no own choice is kept");
});

test("the picker offers Organization default first, names its face, and explains effect, scope and default", () => {
  const { ctx } = fontCtx({ pdm_font_mono:"atkinson", pdm_org_font:JSON.stringify({ ui:"atkinson", mono:"system" }) });
  const html = ctx.fontPickerHTML();
  for (const s of ctx.fontSettings()){
    assert.match(html, new RegExp("<label for='" + s.id + "'>" + s.label + "</label><select id='" + s.id + "'[^>]*><option value='org'"),
      s.label + ": Organization default is the first option");
    assert.match(html, new RegExp("aria-describedby='" + s.id + "-hint'"));
    assert.match(html, new RegExp("<p class='dim' id='" + s.id + "-hint'>[^<]*Default: Organization default"), s.label + " says its default");
  }
  assert.match(html, /<option value='org' selected>Organization default \(Atkinson Hyperlegible Next\)<\/option>/,
    "reading font follows the organization, which uses Atkinson");
  assert.match(html, /<option value='atkinson' selected>Atkinson Hyperlegible Mono<\/option>/, "code font is the member's own choice");
  assert.match(html, /<option value='org'>Organization default \(System default\)<\/option>/);
  assert.match(html, /stored in this browser/i, "says where the member's choice applies");
  assert.match(html, /nothing is downloaded from the internet/, "says the fonts are bundled");
  assert.doesNotMatch(html, /data-org-font|org-font-save/, "members who are not admins do not see the organization picker");
});

test("admins also get the organization default picker, with its scope, default and a save button", () => {
  const { ctx } = fontCtx({ pdm_org_font:JSON.stringify({ ui:"atkinson", mono:"system" }) }, { role:"admin" });
  const html = ctx.fontPickerHTML();
  assert.match(html, /<p class='dim' id='org-font-hint'>[^]*everyone in this organization[^]*Default: System default\.<\/p>/);
  assert.match(html, /<label for='org-font-ui'>Organization reading font<\/label><select id='org-font-ui' data-org-font='ui' aria-describedby='org-font-hint'>/);
  assert.match(html, /<label for='org-font-mono'>Organization code font<\/label><select id='org-font-mono' data-org-font='mono'/);
  assert.match(html, /data-org-font='ui'[^]*?<option value='atkinson' selected>Atkinson Hyperlegible Next/);
  assert.match(html, /data-org-font='mono'[^]*?<option value='system' selected>System default/);
  assert.match(html, /<button type='button' id='org-font-save' disabled>Save organization default<\/button>/,
    "Save waits until the current default has loaded");
  assert.match(html, /<span class='dim' id='org-font-status' role='status'>Loading the current organization default…<\/span>/);
  assert.match(html, /<button type='button' id='org-font-retry' hidden>Retry<\/button>/);
});

// A root element stand-in for the Settings view.
function stubRoot({ fontSelects = [], orgSelects = [], options = {}, save = null, status = null, retry = null } = {}){
  const byId = { "#org-font-save":save, "#org-font-status":status, "#org-font-retry":retry };
  return {
    querySelectorAll: q => (q === "[data-font-key]" ? fontSelects : q === "[data-org-font]" ? orgSelects : []),
    querySelector: q => (q in byId ? byId[q] : options[q] ||
      orgSelects.find(s => q === "[data-org-font='" + s.dataset.orgFont + "']") || null),
  };
}

test("changing a select applies that font, and opening Settings refreshes the organization default", async () => {
  const { ctx, attrs, calls } = fontCtx({}, { api: () => ({ ui:"system", mono:"atkinson" }) });
  const sel = { dataset:{ fontKey:"pdm_font_ui" }, value:"atkinson" };
  await ctx.wireFontPicker(stubRoot({ fontSelects:[sel] }));
  assert.deepEqual(calls.map(c => c.method + " " + c.url), ["GET /settings/reading-font"]);
  assert.deepEqual(attrs, { "data-font-mono":"atkinson" }, "the fetched organization default applies");
  sel.onchange();
  assert.deepEqual(attrs, { "data-font-ui":"atkinson", "data-font-mono":"atkinson" });
});

test("the organization default is fetched, kept for the next page load and applied; a failed fetch keeps the last one", async () => {
  const ok = fontCtx({}, { api: () => ({ ui:"atkinson", mono:"system", extra:1 }) });
  assert.equal(await ok.ctx.refreshOrgFont(), true);
  const cached = JSON.parse(ok.stored.pdm_org_font);
  assert.deepEqual([cached.ui, cached.mono, typeof cached.at], ["atkinson", "system", "number"], "only ui and mono, with when");
  assert.deepEqual(ok.attrs, { "data-font-ui":"atkinson" });
  const down = fontCtx({ pdm_org_font:ORG_BOTH }, { api: () => { throw new Error("network error"); } });
  assert.equal(await down.ctx.refreshOrgFont(), false);
  assert.equal(down.stored.pdm_org_font, ORG_BOTH, "the cached default stays");
  assert.deepEqual(down.attrs, { "data-font-ui":"atkinson", "data-font-mono":"atkinson" });
  const start = app.indexOf("async function enterWorkspace(");
  assert.match(app.slice(start, app.indexOf("\n}", start)), /\n\s*refreshOrgFont\(\);/, "the workspace fetches it when it opens");
});

test("an admin saves the organization default: busy while saving, then applied, shown and confirmed", async () => {
  let during;
  const btn = { textContent:"Save organization default", disabled:false };
  const orgSelects = [{ dataset:{ orgFont:"ui", edited:"1" }, value:"atkinson" }, { dataset:{ orgFont:"mono" }, value:"system" }];
  const orgOpt = { textContent:"Organization default (System default)" };
  const root = stubRoot({ orgSelects, save:btn, options:{ "[data-font-key='pdm_font_ui'] option[value='org']":orgOpt } });
  const { ctx, attrs, stored, calls, notes } = fontCtx({}, { role:"admin", api: (m, u, body) => {
    during = { disabled:btn.disabled, text:btn.textContent };
    ctx.saveOrgFont(root, btn); // a second click while saving sends nothing
    return body;
  } });
  await ctx.saveOrgFont(root, btn);
  assert.deepEqual(JSON.parse(JSON.stringify(calls.map(c => [c.method, c.url, c.body]))), [["PUT", "/settings/reading-font", { ui:"atkinson", mono:"system" }]]);
  assert.deepEqual(during, { disabled:true, text:"Saving…" });
  assert.deepEqual({ disabled:btn.disabled, text:btn.textContent }, { disabled:false, text:"Save organization default" });
  assert.deepEqual([JSON.parse(stored.pdm_org_font).ui, JSON.parse(stored.pdm_org_font).mono], ["atkinson", "system"]);
  assert.deepEqual(attrs, { "data-font-ui":"atkinson" }, "members on Organization default see it at once in this browser");
  assert.equal(orgOpt.textContent, "Organization default (Atkinson Hyperlegible Next)");
  assert(!("edited" in orgSelects[0].dataset), "saved selects follow later refreshes again");
  assert.deepEqual(notes, [["Organization default font saved", "ok"]]);
});

test("a refused save says so, keeps the last default and leaves the button usable", async () => {
  const btn = { textContent:"Save organization default", disabled:false };
  const orgSelects = [{ dataset:{ orgFont:"ui" }, value:"atkinson" }, { dataset:{ orgFont:"mono" }, value:"atkinson" }];
  const { ctx, stored, notes } = fontCtx({ pdm_org_font:JSON.stringify({ ui:"system", mono:"system" }) },
    { role:"admin", api: () => { throw new Error("admin only"); } });
  await ctx.saveOrgFont(stubRoot({ orgSelects, save:btn }), btn);
  assert.deepEqual(notes, [["Organization default font not saved: admin only", "err"]]);
  assert.equal(stored.pdm_org_font, JSON.stringify({ ui:"system", mono:"system" }));
  assert.deepEqual({ disabled:btn.disabled, text:btn.textContent }, { disabled:false, text:"Save organization default" });
});

test("a refreshed organization default updates the labels but not an admin's unsaved edit", () => {
  const { ctx } = fontCtx({ pdm_org_font:ORG_BOTH }, { role:"admin" });
  const uiOpt = { textContent:"" }, monoOpt = { textContent:"" };
  const orgSelects = [{ dataset:{ orgFont:"ui", edited:"1" }, value:"system" }, { dataset:{ orgFont:"mono" }, value:"system" }];
  ctx.syncFontPicker(stubRoot({ orgSelects, options:{
    "[data-font-key='pdm_font_ui'] option[value='org']":uiOpt, "[data-font-key='pdm_font_mono'] option[value='org']":monoOpt } }));
  assert.equal(uiOpt.textContent, "Organization default (Atkinson Hyperlegible Next)");
  assert.equal(monoOpt.textContent, "Organization default (Atkinson Hyperlegible Mono)");
  assert.equal(orgSelects[0].value, "system", "the admin's unsaved edit stays");
  assert.equal(orgSelects[1].value, "atkinson");
});

test("Settings shows the font settings in the Appearance card, right after the theme picker", () => {
  const start = app.indexOf("async function viewSettings(");
  const body = app.slice(start, app.indexOf("\nasync function ", start + 1));
  assert.match(body, /themePickerHTML\(\) \+ fontPickerHTML\(\) \+ "<\/div>"/);
  assert.match(body, /wireFontPicker\(v\);\r?\n\s*wireThemePicker\(v\);/);
});

test("Settings wires the admin picker: Save turns on once the default has loaded, edits are kept, Save sends them", async () => {
  const save = { textContent:"Save organization default", disabled:true }, status = { textContent:"Loading…" }, retry = { hidden:true };
  const orgSelects = [{ dataset:{ orgFont:"ui" }, value:"system" }, { dataset:{ orgFont:"mono" }, value:"system" }];
  const { ctx, calls } = fontCtx({}, { role:"admin", api: (m, u, body) => (m === "GET" ? { ui:"system", mono:"atkinson" } : body) });
  const root = stubRoot({ orgSelects, save, status, retry });
  assert.equal(await ctx.wireFontPicker(root), true);
  assert.deepEqual([save.disabled, status.textContent, retry.hidden], [false, "", true]);
  assert.equal(orgSelects[1].value, "atkinson", "the admin selects show the loaded default");
  orgSelects[0].value = "atkinson";
  orgSelects[0].onchange();
  assert.equal(orgSelects[0].dataset.edited, "1", "a changed select is marked, so a refresh keeps it");
  await save.onclick();
  assert.deepEqual(JSON.parse(JSON.stringify(calls.map(c => [c.method, c.body || null]))),
    [["GET", null], ["PUT", { ui:"atkinson", mono:"atkinson" }]]);
});

test("if the default cannot be loaded, Save stays off, the reason shows and Retry loads it again", async () => {
  let up = false;
  const save = { textContent:"Save organization default", disabled:true }, status = { textContent:"" }, retry = { hidden:true };
  const { ctx, calls } = fontCtx({ pdm_org_font:JSON.stringify({ ui:"system", mono:"system" }) }, { role:"admin",
    api: () => { if (!up) throw new Error("network error"); return { ui:"atkinson", mono:"atkinson" }; } });
  const root = stubRoot({ orgSelects:[{ dataset:{ orgFont:"ui" }, value:"system" }], save, status, retry });
  assert.equal(await ctx.wireFontPicker(root), false);
  assert.equal(save.disabled, true, "saving the cached or fallback value could overwrite a default this browser has not seen");
  assert.match(status.textContent, /could not be loaded/);
  assert.equal(retry.hidden, false);
  up = true;
  await retry.onclick();
  assert.deepEqual([save.disabled, status.textContent, retry.hidden], [false, "", true]);
  assert.equal(calls.length, 2);
});

test("an answer to a fetch that started before a save does not undo the save in this browser", async () => {
  let answer;
  const pendingGet = new Promise(r => { answer = r; });
  const btn = { textContent:"Save organization default", disabled:false };
  const orgSelects = [{ dataset:{ orgFont:"ui" }, value:"atkinson" }, { dataset:{ orgFont:"mono" }, value:"atkinson" }];
  const { ctx, stored, attrs } = fontCtx({}, { role:"admin", api: (m, u, body) => (m === "GET" ? pendingGet : body) });
  const slow = ctx.refreshOrgFont();
  await ctx.saveOrgFont(stubRoot({ orgSelects, save:btn }), btn);
  answer({ ui:"system", mono:"system" });
  assert.equal(await slow, true);
  const cached = JSON.parse(stored.pdm_org_font);
  assert.deepEqual([cached.ui, cached.mono], ["atkinson", "atkinson"], "the saved value stays cached");
  assert.deepEqual(attrs, { "data-font-ui":"atkinson", "data-font-mono":"atkinson" });
  // A cached time in the future (the clock was set back) does not block later answers.
  const later = fontCtx({ pdm_org_font:JSON.stringify({ ui:"atkinson", mono:"atkinson", at:Date.now() + 864e5 }) },
    { api: () => ({ ui:"system", mono:"system" }) });
  await later.ctx.refreshOrgFont();
  assert.equal(JSON.parse(later.stored.pdm_org_font).ui, "system");
});
