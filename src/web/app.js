"use strict";

const el = (id) => document.getElementById(id);

const state = {
  views: [],
  mode: "playback",  // "live" shows only the newest image
  view: "live",
  hours: 24,
  step: 5,
  bbox: "europe",
  scale: "auto",
  outlines: true,    // coastlines and borders together
  theme: "auto",
  speed: 1,          // playback rate, a multiple of BASE_PLAY_MS
  ranged: false,     // timeline is a chosen date range, not a rolling window
  from: null,        // range start, seconds since the epoch, UTC
  to: null,          // range end
  available: null,   // what the receiver holds for this layer and area
  days: new Map(),   // day -> {slots, first, last} for the calendar
  calMonth: null,    // month on show, "YYYY-MM"
  pickA: null,       // first day clicked
  pickB: null,       // second day clicked
  hover: null,       // day under the cursor while one end is down
  frames: [],
  loaded: [],
  bitmaps: [],       // decoded frames, drawn straight to the canvas
  bitmapUrls: [],    // the request each bitmap is a picture of
  bitmapGen: 0,      // retires decodes still in flight when the set is dropped
  index: 0,
  playing: false,    // the ticker is running
  wantPlay: false,   // the viewer has asked for playback
  buffering: false,  // rendering the window before playback starts
  timer: null,
  stalled: 0,
  holding: false,
  token: 0,          // invalidates a frame-list fetch when the selection changes
  prefetchToken: 0,  // separate, so a render pass can be cancelled on its own
  newest: null,      // newest slot the server reports for this source
  sizeKey: "",
  lastFrames: null,
  native: null,
  here: null,
};

/* One frame every 150 ms at 1x, which is 6.7 a second - fast enough for cloud
   to flow, slow enough to follow a front. The speed control divides it. */
const BASE_PLAY_MS = 150;
const SPEEDS = [0.25, 0.5, 1, 2, 4];
const STATUS_MS = 45000;
/* Held at the end of a loop before restarting. Without a pause the wrap looks
   like the weather suddenly jumping backwards. */
const BASE_LOOP_HOLD_MS = 900;
/* How long the animation may wait on a frame that is not ready before giving up
   and moving on. In milliseconds rather than ticks: at 4x a fixed tick count
   would be a quarter of the patience, and the animation would walk straight
   into frames the server had not finished. */
const MAX_STALL_MS = 1500;

function tickMs() {
  return Math.max(20, Math.round(BASE_PLAY_MS / state.speed));
}

/* The pause scales with the speed, or a fast loop spends most of its time
   sitting still - but not below the point where it stops reading as a pause
   and starts reading as a stutter. */
function loopHoldMs() {
  return Math.max(350, Math.round(BASE_LOOP_HOLD_MS / state.speed));
}

function speedLabel(v) {
  return `${v}x`;
}

// --- date range -----------------------------------------------------------

/* `datetime-local` carries no zone: its value is a bare wall-clock string. The
   imagery is UTC and the page never converts it, so these two treat that string
   as UTC in both directions - which is what the label beside the inputs says.
   Reading it any other way would move a chosen hour by the viewer's offset. */
function toInputValue(epoch) {
  const d = new Date(epoch * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return (
    `${d.getUTCFullYear()}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())}` +
    `T${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`
  );
}

/* What the receiver actually holds, so the calendar can mark it. */
async function loadRange() {
  const note = el("rangeNote");
  let r;
  try {
    r = await (
      await fetch(
        `/api/range?view=${encodeURIComponent(state.view)}&bbox=${encodeURIComponent(state.bbox)}`
      )
    ).json();
  } catch (e) {
    note.textContent = "Could not read what is available.";
    note.classList.add("bad");
    return;
  }
  state.available = r;
  state.days = new Map((r.days || []).map((d) => [d.day, d]));

  if (!state.days.size) {
    note.textContent = "This layer holds no imagery.";
    note.classList.add("bad");
    el("calGrid").replaceChildren();
    return;
  }
  note.classList.remove("bad");

  // Open on the month holding the newest imagery, which is what a viewer
  // arriving at the calendar almost always wants.
  const newest = [...state.days.keys()].pop();
  if (!state.calMonth) state.calMonth = newest.slice(0, 7);
  // A selection made before the layer changed may not exist here any more.
  if (state.pickA && !state.days.has(state.pickA)) state.pickA = null;
  if (state.pickB && !state.days.has(state.pickB)) state.pickB = null;
  drawCalendar();
}

const WEEKDAYS = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
const MONTHS = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

function dayKey(y, m, d) {
  const p = (n) => String(n).padStart(2, "0");
  return `${y}-${p(m + 1)}-${p(d)}`;
}

/* Monday-first, which is the convention wherever this data is read. */
function firstColumn(y, m) {
  return (new Date(Date.UTC(y, m, 1)).getUTCDay() + 6) % 7;
}

function drawCalendar() {
  const grid = el("calGrid");
  grid.replaceChildren();
  const [y, m] = state.calMonth.split("-").map(Number);
  const month = m - 1;

  el("calMonth").textContent = `${MONTHS[month]} ${y}`;

  // Navigation stops at the months that actually hold something.
  const keys = [...state.days.keys()];
  const firstMonth = keys[0].slice(0, 7);
  const lastMonth = keys[keys.length - 1].slice(0, 7);
  el("calPrev").disabled = state.calMonth <= firstMonth;
  el("calNext").disabled = state.calMonth >= lastMonth;

  for (const w of WEEKDAYS) grid.append(node("div", "calwd", w));

  const lead = firstColumn(y, month);
  for (let i = 0; i < lead; i++) grid.append(node("div", "calday"));

  const length = new Date(Date.UTC(y, month + 1, 0)).getUTCDate();
  const [lo, hi] = selectedSpan();

  for (let d = 1; d <= length; d++) {
    const key = dayKey(y, month, d);
    const info = state.days.get(key);
    const cell = node("button", "calday", String(d));
    cell.type = "button";
    cell.dataset.day = key;

    if (info) {
      cell.classList.add("has");
      // A full day of Rapid Scan is 288 slots; scale the marker against that
      // so a sparse day looks sparse.
      cell.style.setProperty("--fill-level", Math.max(0.25, Math.min(1, info.slots / 288)).toFixed(2));
      cell.title = `${info.slots} frame${info.slots === 1 ? "" : "s"}`;
      cell.onclick = () => pickDay(key);
      cell.onmouseenter = () => previewTo(key);
      cell.onmouseleave = () => previewTo(null);
      if (lo && hi && key >= lo && key <= hi) {
        cell.classList.add(key === lo || key === hi ? "edge" : "in");
      } else if (key === state.pickA) {
        cell.classList.add("edge");
      }
    } else {
      cell.disabled = true;
    }
    grid.append(cell);
  }
  describeRange();
}

/* The chosen span, ordered, or nothing while only one end is picked. */
function selectedSpan() {
  if (!state.pickA || !state.pickB) return [null, null];
  return state.pickA <= state.pickB
    ? [state.pickA, state.pickB]
    : [state.pickB, state.pickA];
}

/* First click sets one end, second sets the other, a third starts again.
   Clicking the same day twice is a single-day range, which is a normal thing
   to want and would otherwise need two clicks on neighbours. */
function pickDay(key) {
  if (!state.pickA || state.pickB) {
    state.pickA = key;
    state.pickB = null;
  } else {
    state.pickB = key;
  }
  state.hover = null;
  drawCalendar();
  if (state.pickA && state.pickB) {
    // Both ends are in: the calendar has done its job and goes away, leaving
    // the chosen span on the button and the whole stage to the imagery.
    el("dateMenu").open = false;
    applyRange();
  }
}

/* A short label for the button, so the chosen span is readable without opening
   anything. Days in the same month say the month once. */
function dateButtonLabel() {
  const [lo, hi] = selectedSpan();
  if (!lo || !hi) return state.pickA ? `${short(state.pickA)} - ...` : "Pick dates";
  if (lo === hi) return short(lo);
  return lo.slice(0, 7) === hi.slice(0, 7)
    ? `${Number(lo.slice(8))} - ${short(hi)}`
    : `${short(lo)} - ${short(hi)}`;
}

const SHORT_MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                      "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function short(day) {
  const [, m, d] = day.split("-");
  return `${Number(d)} ${SHORT_MONTHS[Number(m) - 1]}`;
}

/* While one end is down, hovering shows what the other would give. */
function previewTo(key) {
  if (!state.pickA || state.pickB) return;
  state.hover = key;
  const lo = state.pickA <= (key || state.pickA) ? state.pickA : key;
  const hi = state.pickA <= (key || state.pickA) ? key : state.pickA;
  for (const cell of el("calGrid").children) {
    if (!cell.dataset || !cell.dataset.day) continue;
    const d = cell.dataset.day;
    cell.classList.toggle(
      "preview",
      Boolean(key) && d > lo && d < hi
    );
  }
  describeRange();
}

/* Step the calendar a month, staying inside what is held. */
function shiftMonth(delta) {
  const [y, m] = state.calMonth.split("-").map(Number);
  const d = new Date(Date.UTC(y, m - 1 + delta, 1));
  state.calMonth = `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}`;
  drawCalendar();
}

function clearRange() {
  state.pickA = null;
  state.pickB = null;
  state.hover = null;
  state.ranged = false;
  state.from = null;
  state.to = null;
  drawCalendar();
  reload();
}

/* The instants a chosen span covers.
   Anchored to the first and last slot the days actually hold rather than to
   nominal midnights, so a day whose reception started at 08:25 gives a range
   beginning at 08:25 and the frame count matches what the calendar promised. */
function spanInstants() {
  const [lo, hi] = selectedSpan();
  if (!lo || !hi) return [null, null];
  const a = state.days.get(lo);
  const b = state.days.get(hi);
  return a && b ? [a.first, b.last] : [null, null];
}

function dayCount(lo, hi) {
  let n = 0;
  for (const key of state.days.keys()) if (key >= lo && key <= hi) n++;
  return n;
}

/* Says what the current selection gives, or what is still needed. Returns
   whether it can be applied. */
function describeRange() {
  const note = el("rangeNote");
  const clear = el("calClear");
  note.classList.remove("bad");
  clear.hidden = !state.pickA;

  el("dateLabel").textContent = dateButtonLabel();
  if (!state.pickA) {
    note.textContent = "Click a day to start, then click another to end.";
    return false;
  }
  if (!state.pickB) {
    // Mid-selection: say what hovering would give, so the preview has words.
    const other = state.hover || state.pickA;
    const lo = state.pickA <= other ? state.pickA : other;
    const hi = state.pickA <= other ? other : state.pickA;
    const days = dayCount(lo, hi);
    note.textContent =
      lo === hi
        ? `${lo} - click again for a single day, or another to end`
        : `${lo} to ${hi} - ${days} day${days === 1 ? "" : "s"} with imagery`;
    return false;
  }

  const [lo, hi] = selectedSpan();
  const [from, to] = spanInstants();
  if (from === null) return false;

  const maxDays = (state.available && state.available.max_days) || 31;
  if (to - from > maxDays * 86400) {
    note.textContent = `At most ${maxDays} days.`;
    note.classList.add("bad");
    return false;
  }
  /* Counted in days that hold imagery, not in elapsed time. Saying "3 days,
     4.0 days" - which is what quoting both gave - reads as a contradiction
     rather than as two different measurements. How many frames that becomes is
     the info panel's business, since only the server knows the interval it
     settled on - a long span is served at a coarser interval than it holds, so
     this is what is there rather than what will be shown. */
  const days = dayCount(lo, hi);
  const slots = [...state.days.values()]
    .filter((d) => d.day >= lo && d.day <= hi)
    .reduce((a, d) => a + d.slots, 0);
  el("dateLabel").textContent = dateButtonLabel();
  note.textContent =
    lo === hi
      ? `${lo} - ${slots} frames available`
      : `${lo} to ${hi} - ${days} days with imagery, ${slots} frames available`;
  return true;
}

function applyRange() {
  if (!describeRange()) return;
  const [from, to] = spanInstants();
  if (from === null) return;
  // Nothing to redo if it is already showing this span.
  if (state.ranged && state.from === from && state.to === to) return;
  state.ranged = true;
  state.from = from;
  state.to = to;
  reload();
}

/* The part of a request that says which span to cover. Shared by the timeline
   and the export so a saved animation is the one that was on screen. */
function windowQuery() {
  return state.ranged && state.from && state.to
    ? `&from=${state.from}&to=${state.to}`
    : `&hours=${state.hours}`;
}

function currentView() {
  return state.views.find((v) => v.id === state.view);
}

function isHrit() {
  const v = currentView();
  return v && ["live", "surface", "composite"].includes(v.style);
}

// --- theme ----------------------------------------------------------------

/* Auto follows the operating system through the CSS media query; the explicit
   choices stamp the root element, which the stylesheet weights above it. */
function applyTheme() {
  const root = document.documentElement;
  if (state.theme === "auto") root.removeAttribute("data-theme");
  else root.dataset.theme = state.theme;
  try {
    localStorage.setItem("eumet.theme", state.theme);
  } catch (e) {
    /* private mode: the choice lasts for this session only */
  }
}

// --- rendering size -------------------------------------------------------

/* Every frame the page holds is a decoded bitmap of four bytes a pixel,
   whatever the PNG compressed to. A full-screen window is around 2200 x 1200,
   which is 10.6 MB a frame - so a 48-hour window at 5 minutes would be nearly
   three gigabytes of bitmaps, which no browser will hand out.

   So the whole window gets a memory budget and the frame size is scaled to fit
   inside it. It only binds when there are many frames: a short window still
   renders at full size. 600 MB measured smooth at the target rate on a 2560 x
   1440 display; the limit is address space, not decode, because each frame is
   decoded once and then only blitted. */
const DECODED_BUDGET_BYTES = 600e6;

function fitWindowBudget(w, h) {
  const frames = Math.max(1, state.frames.length);
  const allowedPixels = DECODED_BUDGET_BYTES / 4 / frames;
  const pixels = w * h;
  if (pixels <= allowedPixels) return { w, h, capped: false };

  const k = Math.sqrt(allowedPixels / pixels);
  const q = (v) => Math.max(300, Math.round((v * k) / 100) * 100);
  return { w: q(w), h: q(h), capped: true };
}

/* Pixel size is quantised so repeated requests reuse the server-side cache
   instead of rendering a slightly different image every time. */
function imageSize() {
  if (state.scale === "native" && state.native) {
    return fitWindowBudget(
      Math.min(3200, Math.max(400, state.native.w)),
      Math.min(2400, Math.max(300, state.native.h))
    );
  }
  const r = el("stage").getBoundingClientRect();
  const mult =
    state.scale === "auto"
      ? Math.min(window.devicePixelRatio || 1, 2)
      : Number(state.scale);
  const q = (v, step, lo, hi) =>
    Math.max(lo, Math.min(hi, Math.round((v * mult) / step) * step));
  return fitWindowBudget(
    q(r.width || 1000, 100, 400, 3200),
    q(r.height || 700, 100, 300, 2400)
  );
}

function sizeKey() {
  const { w, h } = imageSize();
  return `${w}x${h}`;
}

function frameUrl(t) {
  const { w, h } = imageSize();
  const o = state.outlines ? 1 : 0;
  return (
    `/api/frame.png?view=${state.view}&t=${t}&bbox=${state.bbox}` +
    `&w=${w}&h=${h}&coast=${o}&borders=${o}`
  );
}

function applySizeChange() {
  const key = sizeKey();
  if (key === state.sizeKey) return;
  state.sizeKey = key;
  state.loaded = state.frames.map(() => false);
  dropBitmaps();
  state.prefetchToken++;
  el("buffer").style.width = "0%";
  if (state.frames.length) show(state.index);
  refreshInfo();
  // Unlike a rolling refresh, nothing survives a size change - every frame is a
  // different picture now. Rebuild them rather than let the ticker stall.
  if (state.wantPlay && !state.buffering && state.frames.length) prefetch();
}

// --- controls -------------------------------------------------------------

function segmented(container, entries, current, onPick) {
  container.replaceChildren();
  for (const [value, label] of entries) {
    const b = document.createElement("button");
    b.textContent = label;
    if (String(value) === String(current)) b.classList.add("on");
    b.onclick = () => {
      [...container.children].forEach((c) => c.classList.remove("on"));
      b.classList.add("on");
      onPick(value);
    };
    container.appendChild(b);
  }
}

/* A pop-up rather than a segmented control, for the two choices that are lists
   rather than a handful: nine layers and eight intervals. */
function popup(select, entries, current, onPick) {
  select.replaceChildren();
  for (const [value, label] of entries) {
    const o = document.createElement("option");
    o.value = String(value);
    o.textContent = label;
    if (String(value) === String(current)) o.selected = true;
    select.appendChild(o);
  }
  select.onchange = () => onPick(select.value);
}

/* Which intervals are available depends on the area as well as the layer: the
   full-disc service repeats every 15 minutes, so the globe cannot do 5. The
   server reports the valid set with every frame list. */
function buildStepControl(steps) {
  const v = currentView();
  const list = steps || (v && v.steps) || [15];
  if (!list.includes(state.step)) state.step = list[0];
  popup(
    el("steps"),
    list.map((m) => [m, m >= 60 ? `${m / 60} h` : `${m} min`]),
    state.step,
    (m) => {
      state.step = Number(m);
      reload();
    }
  );
}

/* Only the raw-HRIT layers can draw the globe: the NWC SAF products are
   computed on a European sub-window and have nothing to say about the rest of
   the disc, so they are withdrawn rather than silently showing a wider map. */
function availableViews() {
  return state.bbox === "globe"
    ? state.views.filter((v) => ["live", "surface", "composite"].includes(v.style))
    : state.views;
}

function buildViewControl() {
  const list = availableViews();
  if (!list.some((v) => v.id === state.view)) state.view = list[0].id;
  popup(
    el("views"),
    list.map((v) => [v.id, v.label]),
    state.view,
    async (v) => {
      state.view = v;
      buildStepControl();
      await loadNative();
      if (state.ranged) await loadRange();
      reload();
    }
  );
}

async function loadNative() {
  try {
    state.native = await (
      await fetch(`/api/native?view=${state.view}&bbox=${state.bbox}`)
    ).json();
  } catch (e) {
    state.native = null;
  }
  placeMarker();
  refreshInfo();
}

async function init() {
  try {
    state.theme = localStorage.getItem("eumet.theme") || "auto";
    const saved = Number(localStorage.getItem("eumet.speed"));
    // Only a rate the control actually offers, so an old or edited value
    // cannot leave the buttons showing nothing selected.
    if (SPEEDS.includes(saved)) state.speed = saved;
  } catch (e) {
    state.theme = "auto";
  }
  applyTheme();

  let cfg;
  try {
    cfg = await (await fetch("/api/init")).json();
  } catch (e) {
    setInfo("Cannot reach the server.");
    return;
  }
  if (!cfg.views.length) {
    setInfo("No imagery found in the data directories.");
    return;
  }

  state.views = cfg.views;
  state.view = cfg.views[0].id;
  buildViewControl();

  segmented(
    el("windows"),
    [...cfg.windows.map((h) => [h, `${h} h`]), ["range", "Range"]],
    state.hours,
    (h) => {
      if (h === "range") {
        state.ranged = true;
        el("dateMenu").hidden = false;
        loadRange().then(() => {
          // Nothing chosen yet, so open straight onto the calendar rather
          // than leaving a button that has to be found.
          if (!state.pickA) el("dateMenu").open = true;
        });
        return;
      }
      state.ranged = false;
      el("dateMenu").hidden = true;
      el("dateMenu").open = false;
      state.hours = Number(h);
      reload();
    }
  );

  /* The dates apply themselves, like every other control here - picking a
     window or a layer takes effect at once, and a range had been the one thing
     that also wanted a button pressed afterwards. `input` fires on every
     keystroke and spinner tick, so that only updates the note; `change` fires
     when a value is committed, and is what reloads. The debounce covers a
     viewer editing both fields in succession. */
  el("calPrev").onclick = () => shiftMonth(-1);
  el("calNext").onclick = () => shiftMonth(1);
  el("calClear").onclick = clearRange;

  segmented(
    el("scales"),
    [
      ["auto", "Auto"],
      ["1", "1x"],
      ["2", "2x"],
      ["native", "Native"],
    ],
    state.scale,
    (s) => {
      state.scale = s;
      applySizeChange();
    }
  );

  segmented(
    el("speeds"),
    SPEEDS.map((s) => [s, speedLabel(s)]),
    state.speed,
    (s) => {
      state.speed = Number(s);
      applySpeed();
    }
  );

  segmented(
    el("themes"),
    [
      ["auto", "Auto"],
      ["light", "Light"],
      ["dark", "Dark"],
    ],
    state.theme,
    (t) => {
      state.theme = t;
      applyTheme();
    }
  );

  buildStepControl();

  for (const b of el("regions").children) {
    b.onclick = async () => {
      [...el("regions").children].forEach((c) => c.classList.remove("on"));
      b.classList.add("on");
      state.bbox = b.dataset.bbox;
      buildViewControl();
      await loadNative();
      if (state.ranged) await loadRange();
      reload();
    };
  }

  for (const b of el("modes").children) {
    b.onclick = () => {
      [...el("modes").children].forEach((c) => c.classList.remove("on"));
      b.classList.add("on");
      state.mode = b.dataset.mode;
      applyMode();
    };
  }

  el("outlines").onclick = () => {
    state.outlines = !state.outlines;
    el("outlines").classList.toggle("on", state.outlines);
    // Only the picture changes, so the timeline is left alone.
    state.loaded = state.frames.map(() => false);
    dropBitmaps();
    el("buffer").style.width = "0%";
    if (state.frames.length) show(state.index);
  };

  // ?lat=48.21&lon=16.37 pins a fixed spot, which is handy for a bookmark and
  // avoids needing the geolocation permission at all.
  const params = new URLSearchParams(location.search);
  const lat = parseFloat(params.get("lat"));
  const lon = parseFloat(params.get("lon"));
  if (isFinite(lat) && isFinite(lon)) {
    state.here = { lat, lon };
    el("locate").classList.add("on");
  } else {
    try {
      const saved = JSON.parse(localStorage.getItem("eumet.here") || "null");
      if (saved && isFinite(saved.lat) && isFinite(saved.lon)) {
        state.here = saved;
        el("locate").classList.add("on");
      }
    } catch (e) {
      /* nothing stored */
    }
  }

  /* A popover is expected to go away when you click past it or press Escape.
     A bare <details> does neither, and one left hanging open over the image is
     exactly the clutter moving these settings out of the toolbar was meant to
     remove. */
  const menus = () => [el("settingsMenu"), el("dateMenu")];
  document.addEventListener("pointerdown", (e) => {
    for (const m of menus()) {
      if (m.open && !m.contains(e.target)) m.open = false;
    }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    for (const m of menus()) {
      if (m.open) {
        m.open = false;
        m.querySelector("summary").focus();
      }
    }
  });
  // Opening one closes the other; two panels over the image at once is noise.
  for (const m of ["settingsMenu", "dateMenu"]) {
    el(m).addEventListener("toggle", () => {
      if (!el(m).open) return;
      for (const other of menus()) if (other.id !== m) other.open = false;
    });
  }

  el("locate").onclick = locate;
  el("saveFrame").onclick = saveFrame;
  el("saveAnim").onclick = saveAnimation;
  el("play").onclick = togglePlay;

  el("scrub").oninput = (e) => {
    stop();
    show(Number(e.target.value));
  };

  /* Ignored while a control has focus: space is how a focused button is
     pressed and the arrows are how the timeline is nudged, so claiming them
     globally made the keyboard fight the widget under the cursor. */
  window.addEventListener("keydown", (e) => {
    const on = document.activeElement;
    if (on && on !== document.body && on.closest("button, input, select, a")) return;
    if (e.key === " ") { e.preventDefault(); togglePlay(); }
    if (e.key === "ArrowRight") { stop(); show(state.index + 1); }
    if (e.key === "ArrowLeft") { stop(); show(state.index - 1); }
  });

  // Watch the stage itself rather than the window: the sidebar wrapping or a
  // scrollbar appearing changes the render size without a window resize.
  let resizeTimer = null;
  new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      applySizeChange();
      placeMarker();
    }, 250);
  }).observe(el("stage"));

  await loadNative();
  state.sizeKey = sizeKey();
  await reload();

  pollStatus();
  setInterval(pollStatus, STATUS_MS);
  setInterval(paintClock, 30000);
}

// --- location -------------------------------------------------------------

/* The CGMS geostationary projection, mirroring the server so a marker lands on
   the same pixel the renderer would have drawn it on. A linear latitude and
   longitude mapping is right for a map but badly wrong on the disc. */
const R_EQ = 6378.137;
const R_POL = 6356.7523;
const H_SAT = 42164.0;
const RATIO2 = (R_POL * R_POL) / (R_EQ * R_EQ);
const E2 = (R_EQ * R_EQ - R_POL * R_POL) / (R_EQ * R_EQ);

function scanAngles(latDeg, lonDeg, subLonDeg) {
  const lat = (latDeg * Math.PI) / 180;
  const lon = (lonDeg * Math.PI) / 180;
  const sub = ((subLonDeg || 0) * Math.PI) / 180;

  const cLat = Math.atan(RATIO2 * Math.tan(lat));
  const cosC = Math.cos(cLat);
  const sinC = Math.sin(cLat);
  const rl = R_POL / Math.sqrt(1 - E2 * cosC * cosC);

  const d = lon - sub;
  const r1 = H_SAT - rl * cosC * Math.cos(d);
  const r2 = -rl * cosC * Math.sin(d);
  const r3 = rl * sinC;

  if (H_SAT * (H_SAT - r1) < r2 * r2 + ((R_EQ * R_EQ) / (R_POL * R_POL)) * r3 * r3) {
    return null; // behind the limb
  }
  const rn = Math.sqrt(r1 * r1 + r2 * r2 + r3 * r3);
  return [Math.atan(-r2 / r1), Math.asin(-r3 / rn)];
}

/// Displayed rectangle of the image inside the stage, allowing for letterboxing.
function displayedRect() {
  const img = el("frame");
  const stage = el("stage").getBoundingClientRect();
  const box = img.getBoundingClientRect();
  // The canvas carries its intrinsic size, so `object-fit: contain` letterboxes
  // it exactly as it did the <img> it replaced.
  if (!img.width || !img.height) return null;
  const imgAspect = img.width / img.height;
  let dw = box.width;
  let dh = box.height;
  if (imgAspect > box.width / box.height) dh = box.width / imgAspect;
  else dw = box.height * imgAspect;
  return {
    x: box.left - stage.left + (box.width - dw) / 2,
    y: box.top - stage.top + (box.height - dh) / 2,
    w: dw,
    h: dh,
  };
}

function placeMarker() {
  const marker = el("marker");
  const n = state.native;
  const rect = displayedRect();
  if (!state.here || !n || !rect) {
    marker.hidden = true;
    return;
  }
  const { lat, lon } = state.here;
  let px;
  let py;

  if (n.disc) {
    const [cx, cy, half] = n.disc;
    const sa = scanAngles(lat, lon, n.sub_lon);
    if (!sa) {
      marker.hidden = true;
      return;
    }
    const halfRad = (half * Math.PI) / 180;
    const r = Math.min(rect.w, rect.h) / 2 / halfRad;
    px = rect.x + rect.w / 2 + (sa[0] - (cx * Math.PI) / 180) * r;
    py = rect.y + rect.h / 2 + (sa[1] - (cy * Math.PI) / 180) * r;
  } else {
    if (lat < n.lat_min || lat > n.lat_max || lon < n.lon_min || lon > n.lon_max) {
      marker.hidden = true;
      return;
    }
    px = rect.x + ((lon - n.lon_min) / (n.lon_max - n.lon_min)) * rect.w;
    py = rect.y + ((n.lat_max - lat) / (n.lat_max - n.lat_min)) * rect.h;
  }

  if (px < rect.x || px > rect.x + rect.w || py < rect.y || py > rect.y + rect.h) {
    marker.hidden = true;
    return;
  }
  marker.style.left = `${px}px`;
  marker.style.top = `${py}px`;
  marker.hidden = false;
}

function setHere(lat, lon) {
  state.here = { lat, lon };
  try {
    localStorage.setItem("eumet.here", JSON.stringify(state.here));
  } catch (e) {
    /* private mode */
  }
  el("locate").classList.remove("busy");
  el("locate").classList.add("on");
  placeMarker();
}

/* Browser geolocation needs a permission the user may refuse, is unavailable
   over plain http from another machine, and returns nothing at all in some
   desktop builds, so there is always a way through without it. */
function askForLocation() {
  const guess = state.here ? `${state.here.lat}, ${state.here.lon}` : "48.21, 16.37";
  const answer = prompt(
    "Latitude, longitude (decimal degrees, north and east positive):",
    guess
  );
  if (answer === null) {
    el("locate").classList.remove("busy");
    return;
  }
  const parts = answer.split(/[ ,;]+/).filter(Boolean).map(Number);
  if (parts.length < 2 || !parts.every(isFinite)) {
    el("locate").classList.remove("busy");
    setInfo("Could not read that as a latitude and longitude.");
    return;
  }
  setHere(parts[0], parts[1]);
}

function locate() {
  if (state.here) {
    state.here = null;
    try {
      localStorage.removeItem("eumet.here");
    } catch (e) {
      /* nothing to clean up */
    }
    el("locate").classList.remove("on");
    placeMarker();
    return;
  }
  if (!navigator.geolocation || !window.isSecureContext) {
    askForLocation();
    return;
  }
  el("locate").classList.add("busy");
  navigator.geolocation.getCurrentPosition(
    (p) => setHere(p.coords.latitude, p.coords.longitude),
    () => askForLocation(),
    { timeout: 8000, maximumAge: 600000 }
  );
}

// --- export ---------------------------------------------------------------

function download(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}

async function saveFrame() {
  const f = state.frames[state.index];
  if (!f) return;
  const btn = el("saveFrame");
  btn.classList.add("busy");
  try {
    const blob = await (await fetch(frameUrl(f.t))).blob();
    download(blob, `eumet-${state.view}-${f.label.replace(/[: ]/g, "")}.png`);
  } catch (e) {
    setInfo("Could not save that frame.");
  }
  btn.classList.remove("busy");
}

async function saveAnimation() {
  const btn = el("saveAnim");
  if (btn.classList.contains("busy")) return;
  btn.classList.add("busy");
  const original = btn.textContent;
  btn.textContent = "Encoding...";

  // Export at the size on screen: those frames are already in the server's
  // cache, so it only has to decode them rather than re-render. The export
  // endpoint accepts the same range as a single frame, so the size always
  // matches the key those renders were stored under.
  const { w, h } = imageSize();
  // The server keeps the newest 120; saying so beats a button that sits on
  // "Encoding..." with no idea how much is left.
  const count = Math.min(state.frames.length, 120);
  /* The saved file plays at the rate you were watching, rather than a fixed
     one: picking a speed and then getting something else back would make the
     control feel like it only applied to the page. The server clamps to 1-24,
     and GIF timing is in hundredths of a second, so very fast rates land on
     the nearest hundredth. */
  const fps = Math.max(1, Math.min(24, Math.round(1000 / tickMs())));
  setInfo(`Encoding ${count} frames at ${w} x ${h}, ${fps} fps...`);
  const o = state.outlines ? 1 : 0;
  const url =
    `/api/animation.png?view=${state.view}&step=${state.step}${windowQuery()}` +
    `&bbox=${state.bbox}&w=${w}&h=${h}&fps=${fps}&coast=${o}&borders=${o}`;
  try {
    const res = await fetch(url);
    if (!res.ok) throw new Error(await res.text());
    const span = state.ranged
      ? `${toInputValue(state.from)}_${toInputValue(state.to)}`.replace(/[:]/g, "")
      : `${state.hours}h`;
    download(await res.blob(), `eumet-${state.view}-${span}.gif`);
    refreshInfo();
  } catch (e) {
    setInfo(`Animation export failed: ${e.message}`);
  }
  btn.textContent = original;
  btn.classList.remove("busy");
}

// --- freshness ------------------------------------------------------------

function humanAge(seconds) {
  if (seconds < 60) return "just now";
  const m = Math.round(seconds / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.floor(m / 60);
  const rem = m % 60;
  if (h < 24) return rem ? `${h} h ${rem} min ago` : `${h} h ago`;
  return `${Math.floor(h / 24)} d ago`;
}

async function pollStatus() {
  let s;
  try {
    s = await (await fetch("/api/status")).json();
  } catch (e) {
    return;
  }

  const newest = isHrit() ? s.live : s.product;
  const box = el("status");
  box.classList.remove("fresh", "recent", "stale");

  if (!newest) {
    el("statusText").textContent = "No data";
    return;
  }

  const age = Math.max(0, s.now - newest);
  box.classList.add(age < 900 ? "fresh" : age < 1800 ? "recent" : "stale");
  el("statusText").textContent = `Updated ${humanAge(age)}`;

  /* A chosen range is a fixed span of the past: new data does not belong in
     it, and reloading would only reset where you were. Only the rolling
     window follows the leading edge. */
  if (!state.ranged && state.newest !== null && newest > state.newest) {
    refreshFrames({ keepPosition: state.mode !== "live" });
  }
  state.newest = newest;
}

// --- frames ---------------------------------------------------------------

async function reload() {
  stop();
  await loadLegend();
  await refreshFrames({ keepPosition: false });
}

async function refreshFrames({ keepPosition }) {
  const token = ++state.token;
  const currentTime = state.frames[state.index]?.t ?? null;
  const wasAtEnd = state.index >= state.frames.length - 1;

  let data;
  try {
    data = await (
      await fetch(`/api/frames?view=${state.view}&step=${state.step}&bbox=${state.bbox}${windowQuery()}`)
    ).json();
  } catch (e) {
    setInfo("Failed to list frames.");
    return;
  }
  if (token !== state.token) return;

  const previous = state.frames;
  state.frames = data.frames;
  carryBitmaps(previous);
  // Any render pass in flight was working from the old list.
  state.prefetchToken++;
  el("scrub").max = Math.max(0, state.frames.length - 1);
  el("stage").classList.toggle("blank", state.frames.length === 0);
  /* A range too dense for the frame ceiling is served at a coarser interval
     than was asked for, so the picker follows what was actually used rather
     than showing a setting that is not in effect. */
  if (typeof data.step === "number") state.step = data.step;
  if (data.steps) buildStepControl(data.steps);

  if (!state.frames.length) {
    // Nothing left to animate, so playback ends here rather than continuing to
    // claim it is running.
    stop();
    el("counter").textContent = "0 / 0";
    el("clockTime").textContent = "--:--";
    el("clockDate").textContent = "-";
    el("clockAgo").textContent = "-";
    setInfo("No imagery in this window.");
    return;
  }

  let idx = state.frames.length - 1;
  if (keepPosition && currentTime !== null && !wasAtEnd) {
    const found = state.frames.findIndex((f) => f.t === currentTime);
    if (found >= 0) idx = found;
  }

  state.lastFrames = data;
  state.sizeKey = sizeKey();

  if (state.mode === "live") {
    show(state.frames.length - 1);
    el("counter").textContent = "newest";
    el("buffer").style.width = "100%";
    refreshInfo();
    return;
  }

  // Only the frame on screen is rendered; the rest waits for Play.
  show(idx);
  const ready = state.loaded.filter(Boolean).length;
  el("buffer").style.width = `${(ready / state.frames.length) * 100}%`;
  refreshInfo();

  /* If the window rolled while the animation was running, the frames that just
     appeared at the end still have to be fetched. Starting that here keeps the
     prefetch ahead of the ticker; leaving it to the ticker meant stalling on
     each new frame in turn. While Play is still rendering there is already a
     pass running, and it restarts itself with the new list. */
  if (state.wantPlay && !state.buffering && ready < state.frames.length) {
    prefetch();
  }
}

function formatEta(seconds) {
  if (!isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 5) return "a moment left";
  if (seconds < 90) return `${Math.round(seconds)} s left`;
  const m = Math.round(seconds / 60);
  return m < 60 ? `${m} min left` : `${Math.round(m / 60)} h left`;
}

/* Render the window, newest first. Resolves once every frame is in hand, or
   immediately if the pass was cancelled.
   Progress is always reported against the whole window rather than against the
   frames this pass happens to be missing: the frame on screen is already
   rendered, so counting only the outstanding ones left the label one short of
   the frame counter beside it. */
async function prefetch() {
  const token = ++state.prefetchToken;
  const pending = state.frames
    .map((_, i) => i)
    .reverse()
    .filter((i) => !state.loaded[i]);

  const totalFrames = state.frames.length;
  const outstanding = pending.length;
  const concurrency = isHrit() ? 3 : 4;

  let cursor = 0;
  let done = 0; // completed within this pass
  const startedAt = performance.now();
  // Completion times of the most recent frames. A plain average over the whole
  // pass would be dragged down by frames served instantly from cache before the
  // first real render even starts, so the estimate tracks the recent rate.
  const recent = [];

  const remainingSeconds = () => {
    const left = outstanding - done;
    if (left <= 0) return 0;
    // Frames render `concurrency` at a time, so until that many have finished
    // the pipeline is still filling: the first completion looks slow while its
    // siblings are already nearly done, and estimating there overstates the
    // wait by roughly the concurrency factor.
    if (done < Math.min(concurrency, outstanding)) return NaN;

    const perFrame =
      recent.length >= 2
        ? (recent[recent.length - 1] - recent[0]) / (recent.length - 1)
        : (performance.now() - startedAt) / done;
    return (left * perFrame) / 1000;
  };

  const paint = () => {
    const ready = state.loaded.filter(Boolean).length;
    el("buffer").style.width = totalFrames
      ? `${(ready / totalFrames) * 100}%`
      : "0%";
    const counted = el("infoRendered");
    if (counted) counted.textContent = `${ready} / ${totalFrames}`;
    if (state.buffering) {
      const eta = formatEta(remainingSeconds());
      el("playLabel").textContent =
        `Rendering ${ready} / ${totalFrames}` + (eta ? ` - ${eta}` : "");
    }
  };

  if (!outstanding) {
    paint();
    return true;
  }

  const worker = async () => {
    while (cursor < pending.length) {
      const i = pending[cursor++];
      if (token !== state.prefetchToken) return;
      // Decoding here, not at playback time, is the point: by the time the
      // animation runs every frame is a ready-to-blit bitmap.
      await decodeFrame(i).catch(() => null);
      // The list may have rolled under us while that was in flight; marking an
      // index now would be marking a different frame.
      if (token !== state.prefetchToken) return;
      state.loaded[i] = true;
      done++;
      recent.push(performance.now());
      if (recent.length > 8) recent.shift();
      paint();
    }
  };

  await Promise.all(Array.from({ length: concurrency }, worker));
  return token === state.prefetchToken;
}

// --- legend and info ------------------------------------------------------

/* Built as nodes rather than as a string of HTML.
   Class names come out of the product file's `flag_meanings` attribute, and
   `class_labels` turns underscores into spaces - so a token written
   `<img_src=x_onerror=...>` arrives here as a working tag, and assigning it
   through innerHTML ran it. Nothing on this page needs markup from a file, so
   the text goes in as text. */
function node(tag, className, text) {
  const n = document.createElement(tag);
  if (className) n.className = className;
  if (text !== undefined) n.textContent = text;
  return n;
}

/* Colours are formatted by the server as `#rrggbb` and never reach a
   stylesheet as anything else, but they are still checked rather than trusted:
   a style attribute is its own injection surface. */
function safeColour(c) {
  return /^#[0-9a-f]{6}$/i.test(String(c)) ? c : "transparent";
}

async function loadLegend() {
  const box = el("legendCard");
  box.replaceChildren();
  try {
    const lg = await (await fetch(`/api/legend?view=${encodeURIComponent(state.view)}`)).json();
    box.append(node("h2", null, lg.title));

    if (lg.swatches.length) {
      const bar = node("div", "rampbar");
      for (const c of lg.swatches) {
        const s = node("span");
        s.style.background = safeColour(c);
        bar.append(s);
      }
      const labels = node("div", "ramplabels");
      labels.append(node("span", null, lg.lo), node("span", null, lg.hi));
      box.append(bar, labels);
    }

    for (const it of lg.items) {
      const row = node("div", "swatch");
      const dot = node("i");
      dot.style.background = safeColour(it.color);
      row.append(dot, document.createTextNode(String(it.label)));
      box.append(row);
    }

    if (lg.note) box.append(node("p", "note", lg.note));
  } catch (e) {
    box.replaceChildren();
  }
}

/* Messages carry server error text, which carries file paths and whatever the
   underlying error said, so it goes in as text rather than as markup. */
function setInfo(message) {
  const box = el("infoCard");
  box.replaceChildren(node("h2", null, "Status"), node("p", "note", String(message)));
}

/* Rebuilt from live state, so it never shows something the view has moved on
   from. Called after anything that could change one of its rows. */
function refreshInfo() {
  const d = state.lastFrames;
  if (!d) return;
  const { w, h, capped } = imageSize();
  const memory = Math.round((w * h * 4 * Math.max(1, state.frames.length)) / 1e6);
  const step = d.step >= 60 ? `${d.step / 60} h` : `${d.step} min`;
  const native = state.native ? `${state.native.w} x ${state.native.h}` : "-";
  const ready = state.loaded.filter(Boolean).length;
  const rendered =
    state.mode === "live" ? "newest only" : `${ready} / ${state.frames.length}`;
  const v = currentView();

  const rows = [
    ["Layer", v ? v.label : "-"],
    ["Area", state.bbox],
    ["Frames", state.frames.length],
    ["Rendered", rendered],
    // A range names its own span; a window is a length back from the newest.
    state.ranged && d.from && d.to
      ? ["Range", `${toInputValue(d.from)}Z to ${toInputValue(d.to)}Z`.replace(/T/g, " ")]
      : ["Window", `${d.hours} h`],
    ["Interval", step],
    ["Render", `${w} x ${h}${capped ? " (fitted)" : ""}`],
    ["In memory", `${memory} MB`],
    ["Native", native],
    ["Source", isHrit() ? "SEVIRI HRIT" : "NWC SAF"],
  ];

  const dl = node("dl", "rows");
  for (const [label, value] of rows) {
    const dd = node("dd", null, String(value));
    if (label === "Rendered") dd.id = "infoRendered";
    dl.append(node("dt", null, label), dd);
  }
  el("infoCard").replaceChildren(node("h2", null, "This view"), dl);
}

// --- playback -------------------------------------------------------------

function paintClock() {
  const f = state.frames[state.index];
  if (!f) return;
  const [date, time] = f.label.split(" ");
  /* The server sends UTC and the page never converts it: a satellite slot is a
     UTC instant, and shifting it into the reader's zone would put the imagery
     an hour off its own timestamp for anyone not on Greenwich. The trailing Z
     comes off the digits because the card names the zone beside them. */
  el("clockTime").textContent = time.replace("Z", "");
  el("clockDate").textContent = date;
  el("clockAgo").textContent = humanAge(
    Math.max(0, Math.floor(Date.now() / 1000) - f.t)
  );
}

/* Decode a frame once and keep the bitmap.
   Swapping an <img> src re-decodes the PNG whenever the browser's image cache
   has evicted it, which is exactly what happens with a long window of
   full-screen frames - and a 2 megapixel decode per step is what made playback
   crawl. Decoding once and blitting the bitmap makes each step a copy. */
async function decodeFrame(i) {
  if (state.bitmaps[i]) return state.bitmaps[i];
  const f = state.frames[i];
  if (!f) return null;
  const gen = state.bitmapGen;
  const url = frameUrl(f.t);
  const res = await fetch(url);
  if (!res.ok) return null;
  const bmp = await createImageBitmap(await res.blob());
  // A resize, an outline toggle or a new window may have landed while this was
  // in flight; the picture it decoded is no longer the one being shown.
  if (state.frames[i] !== f || state.bitmapGen !== gen) {
    bmp.close();
    return null;
  }
  state.bitmaps[i] = bmp;
  // What this bitmap is actually a picture of. Kept because time alone does not
  // identify a frame - see carryBitmaps.
  state.bitmapUrls[i] = url;
  state.loaded[i] = true;
  return bmp;
}

function paintFrame(bmp) {
  const canvas = el("frame");
  if (canvas.width !== bmp.width || canvas.height !== bmp.height) {
    canvas.width = bmp.width;
    canvas.height = bmp.height;
  }
  const ctx = canvas.getContext("2d", { alpha: true });
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(bmp, 0, 0);
  placeMarker();
}

/* Re-key the decoded frames onto a new frame list.

   A status poll that finds new data refreshes the window, but the window has
   only rolled: at six hours and five minutes, 72 of the 73 timestamps are ones
   already decoded. Dropping them all forced a re-decode of the whole window in
   the middle of a loop, which is what made playback hang every few replays.
   Matching on time keeps every surviving picture and leaves only the genuinely
   new ones to fetch. */
function carryBitmaps(previous) {
  /* Matched on the request the bitmap came from, not on the frame time. Time
     alone does not identify a picture: every layer is derived from the same
     slots, so `live` and `airmass` share all their timestamps, as do Europe and
     Wide. Matching on time carried the old layer's pictures straight into the
     new one - the page said Airmass and showed Live SEVIRI. The URL carries the
     layer, area, size and overlays as well, so it survives exactly when the
     picture really is the same one. */
  const byUrl = new Map();
  previous.forEach((_, i) => {
    if (state.bitmaps[i] && state.bitmapUrls[i]) {
      byUrl.set(state.bitmapUrls[i], state.bitmaps[i]);
    }
  });

  const wanted = state.frames.map((f) => frameUrl(f.t));
  state.bitmaps = wanted.map((u) => byUrl.get(u) || null);
  state.bitmapUrls = wanted.map((u, i) => (state.bitmaps[i] ? u : null));
  state.loaded = state.bitmaps.map((b) => Boolean(b));

  // Whatever no longer belongs to any frame on screen is unreachable.
  const kept = new Set(state.bitmaps);
  for (const b of byUrl.values()) {
    if (!kept.has(b)) b.close();
  }
}

/* Release the decoded frames; they are the bulk of the page's memory.
   Bumping the generation retires any decode still in flight, so a picture of
   the old size or the old overlay setting cannot land in the new array. */
function dropBitmaps() {
  for (const b of state.bitmaps) {
    if (b) b.close();
  }
  state.bitmaps = state.frames.map(() => null);
  state.bitmapUrls = state.frames.map(() => null);
  state.bitmapGen++;
}

function show(i) {
  if (!state.frames.length) return;
  const n = state.frames.length;
  state.index = ((i % n) + n) % n;
  if (state.mode !== "live") {
    el("counter").textContent = `${state.index + 1} / ${n}`;
  }
  el("scrub").value = state.index;
  paintClock();

  const idx = state.index;
  const ready = state.bitmaps[idx];
  if (ready) {
    paintFrame(ready);
    return;
  }
  decodeFrame(idx)
    .then((bmp) => {
      // Only paint if this is still the frame being asked for.
      if (bmp && state.index === idx) paintFrame(bmp);
    })
    // A frame that will not load leaves the previous picture up rather than
    // blanking the stage; the server being briefly unreachable is not fatal.
    .catch(() => null);
}

function announceLoop() {
  const stage = el("stage");
  const badge = el("loopBadge");
  stage.classList.add("looping");
  badge.classList.add("show");
  setTimeout(() => {
    stage.classList.remove("looping");
    badge.classList.remove("show");
  }, loopHoldMs() - 150);
}

function togglePlay() {
  if (state.mode === "live") return;
  if (state.wantPlay) stop();
  else play();
}

/* Play renders the window first, then starts the animation on its own.
   Doing both at once made playback stutter through half-drawn frames while the
   server was still busy building them. */
async function play() {
  if (state.mode === "live" || state.frames.length < 2) return;
  state.wantPlay = true;
  el("playIcon").setAttribute("d", "M6 5h4v14H6zM14 5h4v14h-4z");

  /* Loops because new data landing mid-render retires the pass: the frame list
     it was working from is gone. Stop() clears wantPlay, so that is the only
     way out other than a complete window - it cannot spin. */
  while (state.wantPlay && state.loaded.some((x) => !x)) {
    state.buffering = true;
    el("play").classList.add("busy");
    el("playLabel").textContent = "Rendering...";
    const finished = await prefetch();
    state.buffering = false;
    el("play").classList.remove("busy");
    refreshInfo();
    if (!state.wantPlay) return;
    if (finished) break;
  }
  if (!state.wantPlay) return;

  startTicker();
}

function startTicker() {
  if (state.timer) clearInterval(state.timer);
  state.playing = true;
  state.stalled = 0;
  state.holding = false;
  el("playLabel").textContent = playingLabel();

  const period = tickMs();
  const stallLimit = Math.max(1, Math.round(MAX_STALL_MS / period));

  state.timer = setInterval(() => {
    if (state.holding) return;
    /* A window can empty underneath a running animation - a refresh that finds
       no imagery, a layer with a gap. Without this the modulo below is a
       division by zero, `next` is NaN, and the ticker runs forever against
       nothing while the button still reads "Playing". */
    if (!state.frames.length) {
      stop();
      return;
    }
    const next = (state.index + 1) % state.frames.length;

    if (next === 0 && state.frames.length > 1) {
      state.holding = true;
      announceLoop();
      setTimeout(() => {
        state.holding = false;
        if (state.playing) show(0);
      }, loopHoldMs());
      return;
    }
    if (!state.loaded[next] && state.stalled < stallLimit) {
      state.stalled++;
      return;
    }
    state.stalled = 0;
    show(next);
  }, period);
}

/* The rate is worth showing beside the button: at 4x a whole window is over in
   a couple of seconds, and without a label it is not obvious why. */
function playingLabel() {
  return state.speed === 1 ? "Playing" : `Playing ${speedLabel(state.speed)}`;
}

function idleLabel() {
  if (state.mode === "live") return "Live";
  return state.speed === 1 ? "Play" : `Play ${speedLabel(state.speed)}`;
}

/* Changing speed mid-playback restarts the ticker: setInterval's period is
   fixed once it is running. The frames are already decoded, so this is only a
   new timer, not a new render. */
function applySpeed() {
  try {
    localStorage.setItem("eumet.speed", String(state.speed));
  } catch (e) {
    /* private mode: the choice lasts for this session only */
  }
  if (state.playing) startTicker();
  else if (state.mode !== "live" && !state.buffering) {
    el("playLabel").textContent = idleLabel();
  }
}

function stop() {
  state.wantPlay = false;
  state.playing = false;
  state.buffering = false;
  state.stalled = 0;
  state.holding = false;
  // Abandon any render pass still running.
  state.prefetchToken++;
  el("play").classList.remove("busy");
  el("playIcon").setAttribute("d", "M8 5v14l11-7z");
  el("playLabel").textContent = idleLabel();
  if (state.timer) clearInterval(state.timer);
  state.timer = null;
}

function applyMode() {
  const live = state.mode === "live";
  el("playbar").classList.toggle("live", live);
  el("play").disabled = live;

  /* Live holds the newest image and nothing else - there is no timeline, so a
     span of the past has nothing to be a span of. Leaving Range selectable
     there offers a choice that cannot do anything, and choosing it while
     already in Live would have loaded a range and then shown one frame of it.
     Coming from a range, the rolling window it replaced is what returns. */
  const range = [...el("windows").children].find((b) => b.textContent === "Range");
  if (range) {
    range.disabled = live;
    range.title = live ? "Playback only" : "";
  }
  if (live && state.ranged) {
    state.ranged = false;
    state.from = null;
    state.to = null;
    for (const b of el("windows").children) {
      b.classList.toggle("on", b.textContent === `${state.hours} h`);
    }
  }
  if (live) {
    el("dateMenu").open = false;
    el("dateMenu").hidden = true;
  } else if (state.ranged) {
    el("dateMenu").hidden = false;
  }

  stop();
  el("playLabel").textContent = idleLabel();
  reload();
}

init();
