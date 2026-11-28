// Theme token checks for the web portal.
// Run with: node --test crates/pdm-api/tests/web-ui-tokens.test.cjs
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const STATIC = path.join(__dirname, "../static");
const css = fs.readFileSync(path.join(STATIC, "style.css"), "utf8");
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

// ── style.css parsing ────────────────────────────────────────────────────
const THEMES = ["dark", "light", "slate", "contrast"];
const THEME_SELECTORS = {
  ':root, [data-theme="dark"]': "dark",
  '[data-theme="light"]': "light",
  '[data-theme="slate"]': "slate",
  '[data-theme="contrast"]': "contrast",
};
const SCALE_SELECTOR = ":root";

// Flat list of style rules { selector, body }, where body holds only the rule's own declarations.
// Conditional at-rules (@media, @supports, @layer, @container …) and nested rules are walked;
// @keyframes is skipped.
function parseRules(src){
  const out = [];
  (function walk(text, selector){
    let i = 0, own = "";
    while (i < text.length){
      const open = text.indexOf("{", i);
      if (open < 0){ own += text.slice(i); break; }
      const chunk = text.slice(i, open), cut = chunk.lastIndexOf(";") + 1;
      own += chunk.slice(0, cut);
      const prelude = chunk.slice(cut).trim().replace(/\s+/g, " ");
      let depth = 1, j = open + 1;
      while (j < text.length && depth > 0){
        if (text[j] === "{") depth++;
        else if (text[j] === "}") depth--;
        j++;
      }
      const body = text.slice(open + 1, j - 1);
      if (prelude.startsWith("@")){ if (!/^@keyframes\b/.test(prelude)) walk(body, null); }
      else walk(body, prelude);
      i = j;
    }
    if (selector !== null && own.trim()) out.push({ selector, body:own });
  })(src.replace(/\/\*[\s\S]*?\*\//g, ""), null);
  return out;
}
function declarations(body){
  const d = new Map();
  for (const part of body.split(";")){
    const m = part.match(/^\s*([-\w]+)\s*:\s*([\s\S]+?)\s*$/);
    if (m) d.set(m[1], m[2]);
  }
  return d;
}
const RULES = parseRules(css);
const isTokenBlock = r => Boolean(THEME_SELECTORS[r.selector]) || r.selector === SCALE_SELECTOR;
// Declarations of every rule with this exact selector (e.g. a later @media copy), later ones winning.
const rule = selector => {
  const found = RULES.filter(r => r.selector === selector);
  assert(found.length > 0, "a rule for " + selector);
  return new Map(found.flatMap(r => [...declarations(r.body)]));
};
const themeBlocks = name => RULES.filter(r => THEME_SELECTORS[r.selector] === name);
const scaleBlocks = RULES.filter(r => r.selector === SCALE_SELECTOR);
const tokensOf = r => [...declarations(r.body).keys()].filter(k => k.startsWith("--"));
const TOKENS = new Map(THEMES.map(t => [t, new Map(themeBlocks(t).flatMap(r =>
  [...declarations(r.body)].filter(([k]) => k.startsWith("--"))))]));
const THEME_TOKEN_NAMES = new Set([...TOKENS.values()].flatMap(m => [...m.keys()]));

test("each theme has exactly one token block", () => {
  for (const t of THEMES) assert.equal(themeBlocks(t).length, 1, t + " theme blocks");
  assert.equal(scaleBlocks.length, 1, "one plain :root block for the scale tokens");
});

test("every theme block declares every theme token", () => {
  for (const t of THEMES){
    const missing = [...THEME_TOKEN_NAMES].filter(k => !TOKENS.get(t).has(k));
    assert.deepEqual(missing, [], t + " theme block is missing tokens that another theme defines");
  }
  // Names that other portal changes are told to use (with a fallback until this one lands).
  for (const name of ["--on-acc", "--primary-bg", "--primary-border", "--primary-hover", "--primary-fg",
    "--seg-on-bg", "--field-line", "--mark-dim", "--backdrop", "--paper", "--canvas", "--draw-sheet",
    "--draw-grain", "--draw-text", "--draw-profile", "--draw-hole", "--draw-ignore", "--text-1",
    "--text-2", "--text-3", "--life-wip", "--life-in-review", "--life-released", "--life-draft",
    "--life-obsolete", "--life-deprecated", "--life-lock"])
    assert(THEME_TOKEN_NAMES.has(name), "theme token " + name + " is defined");
});

test("the shared :root block holds only scale tokens, never a theme token", () => {
  for (const [name] of declarations(scaleBlocks[0].body)){
    assert(name.startsWith("--"), ":root declares " + name + "; only scale tokens belong there");
    assert(!THEME_TOKEN_NAMES.has(name), name + " is in a theme block and in :root (the later one wins in every theme)");
    assert.match(name, /^--(sp|r|fs|lh|font)-/, name + " in :root is not a spacing, radius, type, line-height or font token");
  }
  // No other rule redefines a theme token: it would silently override some themes.
  for (const r of RULES){
    if (isTokenBlock(r)) continue;
    for (const name of tokensOf(r)) assert(!THEME_TOKEN_NAMES.has(name), r.selector + " redefines " + name);
  }
});

test("every var() without a fallback names a declared custom property", () => {
  const declared = new Set(RULES.flatMap(tokensOf));
  const missing = [];
  for (const r of RULES) for (const [, value] of declarations(r.body))
    for (const m of value.matchAll(/var\(\s*(--[\w-]+)\s*([,)])/g))
      if (m[2] === ")" && !declared.has(m[1])) missing.push(r.selector + " → " + m[1]);
  assert.deepEqual(missing, []);
});

// ── Color literals stay in the token blocks ─────────────────────────────
// Literals still owned by other open changes, by rule and property; any other literal fails,
// including a second use of an allowed one. Remove an entry once its rule uses a token.
const ALLOWED_LITERALS = [
  { selector:".thumb::after, .thumb-sm::after", prop:"color", literal:"#3a485e", use:"--mark-dim" },
  { selector:".dcard .thumb-lg::after", prop:"color", literal:"#3a485e", use:"--mark-dim" },
  { selector:".modal-backdrop", prop:"background", literal:"rgba(5,8,12,.6)", use:"--backdrop" },
  { selector:".detailPane .backdrop", prop:"background", literal:"rgba(5,8,12,.5)", use:"--backdrop (alpha .6)" },
  { selector:".pv-canvas img, .pv-canvas svg", prop:"box-shadow", literal:"rgba(0,0,0,.4)", use:"a shadow token" },
  { selector:".pv-canvas iframe", prop:"background", literal:"#fff", use:"--paper" },
  { selector:".ln-preview", prop:"background", literal:"#f6f8fb", use:"--paper (white)" },
  { selector:".nc-preview", prop:"background", literal:"#111", use:"--canvas" },
  { selector:".nc-pickthumb", prop:"background", literal:"#111", use:"--canvas" },
  { selector:".nc-partimg", prop:"background", literal:"#111", use:"--canvas" },
  { selector:".nc-legend", prop:"color", literal:"#ccc", use:"a text token on --canvas" },
  { selector:".nc-legend", prop:"background", literal:"rgba(0,0,0,.5)", use:"--canvas with transparency" },
];
// var(--token, fallback) is fine once --token is a theme token: the fallback is never used.
function dropKnownFallbacks(value){
  let out = "", i = 0;
  while (i < value.length){
    const k = value.indexOf("var(", i);
    if (k < 0){ out += value.slice(i); break; }
    out += value.slice(i, k);
    let depth = 0, j = k + 3;
    for (; j < value.length; j++){
      if (value[j] === "(") depth++;
      else if (value[j] === ")" && --depth === 0) break;
    }
    const inner = value.slice(k + 4, j);
    const name = inner.split(",")[0].trim();
    out += THEME_TOKEN_NAMES.has(name) ? "var(" + name + ")" : "var(" + dropKnownFallbacks(inner) + ")";
    i = j + 1;
  }
  return out;
}
const NAMED_COLORS = ("aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue " +
  "blueviolet brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan " +
  "darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki darkmagenta darkolivegreen " +
  "darkorange darkorchid darkred darksalmon darkseagreen darkslateblue darkslategray darkslategrey " +
  "darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey dodgerblue firebrick floralwhite " +
  "forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray green greenyellow grey honeydew hotpink " +
  "indianred indigo ivory khaki lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan " +
  "lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue " +
  "lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon " +
  "mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen " +
  "mediumturquoise mediumvioletred midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive " +
  "olivedrab orange orangered orchid palegoldenrod palegreen paleturquoise palevioletred papayawhip " +
  "peachpuff peru pink plum powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon " +
  "sandybrown seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen " +
  "steelblue tan teal thistle tomato turquoise violet wheat white whitesmoke yellow yellowgreen").split(" ");
const LITERAL = new RegExp("#[0-9a-fA-F]{3,8}\\b|\\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\\([^)]*\\)" +
  "|(?<![\\w-])(?:" + NAMED_COLORS.join("|") + ")(?![\\w(-])", "gi");
const literalsIn = value => (dropKnownFallbacks(value)
  .replace(/"[^"]*"|'[^']*'/g, "\"\"").replace(/var\(--[\w-]+\)/g, "var()")
  .match(LITERAL) || []).map(l => l.replace(/\s+/g, "").toLowerCase());

test("color literals appear only in the theme token blocks", () => {
  const found = [];
  for (const r of RULES){
    if (THEME_SELECTORS[r.selector]) continue;
    for (const [prop, value] of declarations(r.body)) for (const literal of literalsIn(value))
      if (!ALLOWED_LITERALS.some(a => a.selector === r.selector && a.prop === prop && a.literal === literal))
        found.push(r.selector + " { " + prop + " } → " + literal);
  }
  assert.deepEqual(found, [], "use a theme token (add it to all four theme blocks if it is new)");
});

test("the literal check sees fallbacks of unknown tokens and other color forms", () => {
  assert.deepEqual(literalsIn("var(--on-acc, #fff)"), []);
  assert.deepEqual(literalsIn("1px solid var(--line)"), []);
  assert.deepEqual(literalsIn("var(--no-such-token, #fff)"), ["#fff"]);
  assert.deepEqual(literalsIn("color-mix(in srgb, var(--acc) 8%, #000)"), ["#000"]);
  assert.deepEqual(literalsIn("1px solid White"), ["white"]);
  assert.deepEqual(literalsIn("oklch(70% 0.1 250)"), ["oklch(70%0.1250)"]);
  assert.deepEqual(literalsIn("nowrap"), []);
  assert.deepEqual(literalsIn("transform .18s ease"), []);
  assert.deepEqual(parseRules("@layer x { .a { color:#123456 } } .b { .c & { background:red } }")
    .map(r => r.selector + ":" + literalsIn([...declarations(r.body).values()].join(" ")).join()),
    [".a:#123456", ".c &:red"]);
});

// ── Font stacks stay in the font tokens (guideline Y1) ──────────────────
// font-family is exactly var(--font-ui) or var(--font-mono); the font shorthand is style, weight, size and
// line height followed by one of them. The stacks live in :root and in the html[data-font-*] rules
// that a user's font choice turns on.
const FONT_TOKEN = /var\(--font-(?:ui|mono)\)/.source;
const FONT_OK = {
  "font-family": new RegExp("^" + FONT_TOKEN + "$"),
  "font": new RegExp(/^(?:(?:normal|italic|oblique|bold|bolder|lighter|small-caps|[1-9]00)\s+)*/.source +
    /[\d.]+(?:px|r?em|%)(?:\/[\d.]+(?:px|r?em|%)?)?\s+/.source + FONT_TOKEN + "$"),
};
const fontOk = (prop, value) => /^(?:inherit|initial|unset|revert)$/.test(value) || FONT_OK[prop].test(value);
function fontStacksIn(name, src){
  const found = [];
  for (const r of parseRules(src)) for (const [prop, value] of declarations(r.body)){
    if (prop.startsWith("--font-")){
      if (r.selector !== SCALE_SELECTOR && !/^html\[data-font-(?:ui|mono)="[\w-]+"\]$/.test(r.selector))
        found.push(name + ": " + r.selector + " sets " + prop);
    } else if (FONT_OK[prop] && !fontOk(prop, value)) found.push(name + ": " + r.selector + " { " + prop + ":" + value + " }");
  }
  return found;
}
// Inline styles and attributes built in markup or scripts.
function inlineFontsIn(name, src){
  const found = [];
  for (const m of src.matchAll(/font-family\s*[:=]|\.fontFamily\b/gi)) found.push(name + ": " + m[0]);
  for (const m of src.matchAll(/\bfont\s*:\s*([^;"'<>]+)/g)) if (!fontOk("font", m[1].trim())) found.push(name + ": " + m[0]);
  return found;
}
const styleBlocks = html => [...html.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map(m => m[1]).join("\n");

test("font-family and font use --font-ui or --font-mono, never a stack of their own", () => {
  const read = file => fs.readFileSync(path.join(STATIC, file), "utf8");
  const onshape = read("onshape.html");
  const found = [
    ...fontStacksIn("style.css", css),
    ...fontStacksIn("editors.css", read("editors.css")),
    ...fontStacksIn("index.html", styleBlocks(indexHtml)),
    ...fontStacksIn("onshape.html", styleBlocks(onshape)),
  ];
  for (const [name, src] of [["app.js", app], ["editors.js", read("editors.js")], ["index.html", indexHtml], ["onshape.html", onshape]])
    found.push(...inlineFontsIn(name, src));
  assert.deepEqual(found, [], "use var(--font-ui) or var(--font-mono); a new face belongs in the font tokens");
  // The check itself: tokens and inherit pass; stacks, names beside or inside a token, and stray token definitions fail.
  assert.deepEqual(fontStacksIn("t", ".a { font:600 10px var(--font-mono) } .b { font:14px/1.5 var(--font-ui) }" +
    " .c { font:inherit } .d { font-family:var(--font-mono) } .e { font:11px ui-monospace,monospace }" +
    " .f { font-family:\"Comic Sans MS\", var(--font-ui) } .g { font:12px Arial, var(--font-mono) }" +
    " .h { font-family:var(--font-ui, Arial) } .i { --font-ui:serif } :root { --font-mono:monospace }").map(f => f.split(" ")[1]),
  [".e", ".f", ".g", ".h", ".i"]);
  assert.deepEqual(inlineFontsIn("t", "style='font:12px Arial' <text font-family=\"monospace\"> el.style.fontFamily = x;" +
    " style='font:11px var(--font-mono)' toast(\"Font: saved\")").length, 3);
});

// The fixes this change makes, so a later edit cannot quietly put the old values back.
test("rules use the theme tokens for buttons, fields and accent fills", () => {
  const uses = (selector, prop, token) =>
    assert.match(rule(selector).get(prop) || "", new RegExp("var\\(" + token + "\\)"), selector + " " + prop);
  uses("button.primary", "background", "--primary-bg");
  uses("button.primary", "border-color", "--primary-border");
  uses("button.primary", "color", "--primary-fg");
  uses("button.primary:hover", "background", "--primary-hover");
  uses(".seg button.on", "background", "--seg-on-bg");
  for (const s of [".seg", "input, select, textarea", ".chip", ".densityBar"]) uses(s, "border", "--field-line");
  assert(!rule(".kebab-btn").has("opacity"), "row menus are not faded until hover");
  // Text on an accent fill uses its ink: these rules by name, and any other accent fill that sets a color.
  const ink = value => dropKnownFallbacks(value || "");
  for (const s of [".emptyState .cta", ".nc-tab.on", ".nc-sheetrow.sel", ".nc-partrow.sel"]){
    assert.equal(ink(rule(s).get("background")), "var(--acc)", s + " background");
    assert.equal(ink(rule(s).get("color")), "var(--on-acc)", s + " text on --acc");
  }
  for (const r of RULES){
    const d = declarations(r.body);
    if (ink(d.get("background")) === "var(--acc)" && d.has("color"))
      assert.equal(ink(d.get("color")), "var(--on-acc)", r.selector + " text on --acc");
  }
});

// ── WCAG 2.x contrast of the token values ───────────────────────────────
function resolve(theme, name, seen = []){
  assert(!seen.includes(name), theme + ": token cycle " + seen.concat(name).join(" → "));
  const v = TOKENS.get(theme).get(name);
  assert(v !== undefined, theme + ": " + name + " is not defined");
  const ref = v.match(/^var\((--[\w-]+)\)$/);
  if (ref) return resolve(theme, ref[1], seen.concat(name));
  const hex = v.match(/^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/);
  assert(hex, theme + ": " + name + " = " + v + " is not a solid hex color");
  const h = hex[1].length === 3 ? hex[1].replace(/./g, c => c + c) : hex[1];
  return [0, 2, 4].map(i => parseInt(h.slice(i, i + 2), 16));
}
function luminance(rgb){
  const [r, g, b] = rgb.map(c => { c /= 255; return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a, b){
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}
const SURFACES = ["--bg", "--panel", "--panel2", "--head", "--hover", "--active"];
const TEXT_ON_SURFACES = ["--fg", "--dim", "--text-1", "--text-2", "--text-3", "--acc", "--ok", "--err", "--warn",
  "--life-wip", "--life-in-review", "--life-released", "--life-draft", "--life-obsolete", "--life-deprecated", "--life-lock"];
const PAIRS = [
  ...TEXT_ON_SURFACES.flatMap(fg => SURFACES.map(bg => [fg, bg, 4.5])),
  ["--on-acc", "--acc", 4.5],               // text on accent buttons and selected rows
  ["--primary-fg", "--primary-bg", 4.5],    // primary buttons
  ["--primary-fg", "--primary-hover", 4.5],
  ["--acc", "--seg-on-bg", 4.5],            // selected segment
  ["--draw-text", "--canvas", 4.5],         // labels on drawing previews
  ["--acc", "--bg", 3],                     // focus ring (non-text)
  ["--acc", "--panel", 3],
  ["--field-line", "--panel", 3],           // form field edges (non-text)
  ["--field-line", "--panel2", 3],
  ["--field-line", "--bg", 3],
  ["--draw-sheet", "--canvas", 3], ["--draw-grain", "--canvas", 3],
  ["--draw-profile", "--canvas", 3], ["--draw-hole", "--canvas", 3], ["--draw-ignore", "--canvas", 3],
];

test("token pairs meet WCAG AA in every theme", () => {
  const fails = [];
  for (const t of THEMES) for (const [fg, bg, min] of PAIRS){
    const r = contrast(resolve(t, fg), resolve(t, bg));
    if (r < min) fails.push(t + ": " + fg + " on " + bg + " = " + r.toFixed(2) + " (needs " + min + ")");
  }
  assert.deepEqual(fails, []);
});

test("contrast helper matches known WCAG values", () => {
  assert.equal(contrast([255, 255, 255], [0, 0, 0]).toFixed(2), "21.00");
  assert.equal(contrast([255, 255, 255], [0x5a, 0xa9, 0xff]).toFixed(2), "2.46");
});

// ── Saved theme applied before first paint ───────────────────────────────
function headThemeScript(){
  const headEnd = indexHtml.indexOf("</head>"), bodyStart = indexHtml.indexOf("<body");
  assert(headEnd > 0 && bodyStart > headEnd, "index.html has a <head> before <body>");
  const scripts = [...indexHtml.slice(0, headEnd).matchAll(/<script>([\s\S]*?)<\/script>/g)]
    .map(m => m[1]).filter(s => s.includes("pdm_theme"));
  assert.equal(scripts.length, 1, "one inline <head> script reads pdm_theme");
  return scripts[0];
}
function runHeadScript(stored, opts = {}){
  const attrs = { "data-theme":"dark" };
  const localStorage = { getItem(k){ if (opts.throws) throw new Error("storage blocked"); return k === "pdm_theme" ? stored : null; } };
  const document = { documentElement:{ setAttribute(k, v){ attrs[k] = v; }, getAttribute(k){ return attrs[k]; } } };
  const context = vm.createContext({ localStorage, document });
  vm.runInContext(headThemeScript(), context);
  assert.deepEqual(Object.keys(context).sort(), ["document", "localStorage"], "the script leaves no globals behind");
  return attrs["data-theme"];
}
// Theme ids from app.js's THEMES list (ids only, so the rest of each entry can change freely).
const appThemes = () => [...app.match(/const THEMES = \[([\s\S]*?)\n\];/)[1].matchAll(/\bid\s*:\s*"([\w-]+)"/g)]
  .map(m => ({ id:m[1] }));

test("index.html applies the saved theme in <head>, before first paint", () => {
  for (const t of THEMES) assert.equal(runHeadScript(t), t);
  assert.equal(runHeadScript("neon"), "dark", "unknown value keeps the default");
  assert.equal(runHeadScript(null), "dark", "nothing saved keeps the default");
  assert.equal(runHeadScript("light", { throws:true }), "dark", "blocked storage does not stop the script");
  assert.match(indexHtml, /<html[^>]*data-theme="dark"/, "dark stays the no-script default");
});

test("index.html, app.js and style.css agree on the theme ids and default", () => {
  const ids = appThemes().map(t => t.id);
  assert.deepEqual([...ids].sort(), [...THEMES].sort(), "app.js THEMES vs style.css theme blocks");
  const accepted = [...ids, "neon", "Dark", ""].filter(id => runHeadScript(id) === id);
  assert.deepEqual(accepted.sort(), [...ids].sort(), "head script accepts exactly the app.js theme ids");
  // app.js falls back the same way when it re-applies the theme.
  const attrs = {}, store = {};
  const ctx = loadFunctions(["currentTheme", "applyTheme"], {
    THEMES:appThemes(),
    document:{ documentElement:{ setAttribute(k, v){ attrs[k] = v; } } },
    localStorage:{ getItem:() => null, setItem(k, v){ store[k] = v; } },
  });
  assert.equal(ctx.currentTheme(), "dark", "app.js default matches the <head> script default");
  ctx.applyTheme("neon");
  assert.equal(attrs["data-theme"], "dark");
  ctx.applyTheme("slate");
  assert.equal(attrs["data-theme"], "slate");
  assert.equal(store.pdm_theme, "slate");
});
