#!/usr/bin/env node
// Behaviour check for the web UI's live-apply / save merge (D34).
//
// The page used to PUT its whole document on every mixer nudge, which reverted
// anything that changed out of band since page load — the hardware voice select
// most visibly, and `takes` worst of all (a stale body deleted a take recorded
// after the page loaded, because swap adoption only protects takes finalized in
// the last 30 s). The fix is "never PUT a document we composed": start from the
// store's current document and copy only our own fields on top.
//
// That logic lives inline in index.html (the UI is a single file with no build
// step), so this script extracts it between the `live-merge` sentinels, evals
// it, and exercises it as a unit. It also syntax-checks the whole inline script,
// which catches the class of mistake CI otherwise can't see in a .html file.
//
//   node scripts/live-merge-check.mjs       # exit 0 = pass

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const HTML = join(ROOT, "crates/pocketstage-server/web/index.html");
const html = readFileSync(HTML, "utf8");

let failures = 0;
function check(name, fn) {
  try {
    fn();
    console.log(`  ok  ${name}`);
  } catch (e) {
    failures++;
    console.log(`FAIL  ${name}\n      ${e.message}`);
  }
}
function eq(a, b, what = "") {
  const x = JSON.stringify(a), y = JSON.stringify(b);
  if (x !== y) throw new Error(`${what} got ${x}, want ${y}`);
}

// ---------------------------------------------------------------- extraction

const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
if (!scripts.length) throw new Error("no inline <script> found");

// Whole-script syntax check: compiles, never runs (it touches the DOM).
try {
  new Function(scripts.join("\n"));
  console.log(`  ok  inline script parses (${scripts.join("").split("\n").length} lines)`);
} catch (e) {
  failures++;
  console.log(`FAIL  inline script does not parse: ${e.message}`);
}

const START = "// ---8< live-merge:start";
const END = "// ---8< live-merge:end";
const from = html.indexOf(START), to = html.indexOf(END);
if (from < 0 || to < 0) throw new Error("live-merge sentinels missing from index.html");
const block = html.slice(html.indexOf("\n", from) + 1, to);
const { mergeOnto, registerLiveFields, applyLiveFields, UI_OWNED } =
  new Function(`${block}\nreturn { mergeOnto, registerLiveFields, applyLiveFields, UI_OWNED };`)();

// Fixture shaped like a real document, minus the noise.
const doc = (mut = () => {}) => {
  const d = {
    version: 1,
    project: {
      name: "Untitled", bpm: 120, loop_bars: 4,
      tracks: [
        { name: "Piano", instrument: { gm_program: 0 }, clips: [], gain: 1, pan: 0, mute: false },
        { name: "Bass", instrument: { gm_program: 33 }, clips: [], gain: 0.9, pan: 0, mute: false },
      ],
      scenes: [{ name: "Intro" }],
      takes: [{ name: "Take 1", muted: false, events: [] }],
      active_voice: "drums",
      fx_reverb: 0.9, fx_chorus: 0, master_gain: 0.3,
    },
  };
  mut(d);
  return d;
};

// ---------------------------------------------------------------- mergeOnto

check("save keeps the server's voice instead of reverting it", () => {
  const ours = doc();                                  // page-load copy
  const fresh = doc((d) => { d.project.active_voice = { program: 42 }; });
  ours.project.tracks[0].gain = 0.4;                    // our mixer edit
  const out = mergeOnto(ours, fresh).project;
  eq(out.active_voice, { program: 42 }, "active_voice:");
  eq(out.tracks[0].gain, 0.4, "our gain edit:");
});

check("save keeps a take recorded after the page loaded", () => {
  const ours = doc();
  const fresh = doc((d) => d.project.takes.push({ name: "Take 2", muted: false, events: [] }));
  ours.project.tracks[1].mute = true;
  eq(mergeOnto(ours, fresh).project.takes.map((t) => t.name), ["Take 1", "Take 2"]);
});

check("unrendered future fields survive a save", () => {
  const ours = doc();
  const fresh = doc((d) => { d.project.something_new = { deep: 1 }; });
  eq(mergeOnto(ours, fresh).project.something_new, { deep: 1 });
});

check("UI_OWNED excludes takes and active_voice by name", () => {
  for (const k of ["takes", "active_voice"]) {
    if (UI_OWNED.includes(k)) throw new Error(`${k} must not be page-owned`);
  }
  for (const k of ["tracks", "scenes", "bpm", "master_gain"]) {
    if (!UI_OWNED.includes(k)) throw new Error(`${k} should be page-owned`);
  }
});

// ------------------------------------------------------------ live fields

check("live flush writes only its own mixer values onto the fresh doc", () => {
  registerLiveFields("mixer", []);
  registerLiveFields("takes", []);
  const fresh = doc((d) => { d.project.active_voice = { program: 73 }; });
  const patches = [];
  // Exactly what renderMixer registers: index + name guard, value from control.
  const own = (ti, name, key, get) => patches.push((d) => {
    const x = (d.project.tracks || [])[ti];
    if (x && x.name === name) x[key] = get();
  });
  own(0, "Piano", "gain", () => 0.25);
  registerLiveFields("mixer", patches);
  applyLiveFields(fresh);
  eq(fresh.project.tracks[0].gain, 0.25);
  eq(fresh.project.active_voice, { program: 73 }, "voice survived the flush:");
  eq(fresh.project.takes.length, 1, "takes untouched:");
});

check("patchers skip a track that was renamed or deleted server-side", () => {
  const patches = [];
  const own = (ti, name, key, get) => patches.push((d) => {
    const x = (d.project.tracks || [])[ti];
    if (x && x.name === name) x[key] = get();
  });
  own(1, "Bass", "gain", () => 0.1);
  registerLiveFields("mixer", patches);

  const renamed = doc((d) => { d.project.tracks[1].name = "Upright"; });
  applyLiveFields(renamed);
  eq(renamed.project.tracks[1].gain, 0.9, "no write to a slot we misidentify:");

  registerLiveFields("mixer", []);   // renderer ran again with fewer controls
});

check("take mute patchers address takes by name, not position", () => {
  const patches = [];
  for (const t of [{ name: "Take 2", muted: true }]) {
    patches.push((d) => {
      const x = (d.project.takes || []).find((y) => y.name === t.name);
      if (x) x.muted = t.muted;
    });
  }
  registerLiveFields("takes", patches);
  const fresh = doc((d) => {
    d.project.takes = [
      { name: "Take 2", muted: false, events: [] },
      { name: "Take 1", muted: false, events: [] },
    ];
  });
  applyLiveFields(fresh);
  eq(fresh.project.takes.map((t) => [t.name, t.muted]), [["Take 2", true], ["Take 1", false]]);
});

check("re-registering a namespace drops stale controls", () => {
  registerLiveFields("mixer", [(d) => { d.project.tracks[0].gain = 0.11; }]);
  registerLiveFields("mixer", []);           // e.g. after a project switch
  const fresh = doc();
  applyLiveFields(fresh);
  eq(fresh.project.tracks[0].gain, 1);
});

check("applyLiveFields tolerates a malformed document", () => {
  registerLiveFields("mixer", [(d) => { d.project.tracks[0].gain = 0; }]);
  eq(applyLiveFields(null), null);
  eq(applyLiveFields({}), {});
});

console.log(failures ? `\n${failures} check(s) FAILED` : "\nlive-merge checks all green");
process.exit(failures ? 1 : 0);
