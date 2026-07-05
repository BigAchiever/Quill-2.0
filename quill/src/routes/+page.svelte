<script lang="ts">
  import { onMount } from "svelte";
  import { fade } from "svelte/transition";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  const PILL_W = 250, PILL_H = 28;
  const PANEL_W = 600, PANEL_H = 460;

  let expanded = $state(false);
  let closing = $state(false); // content fading out, before the window shrinks
  let view = $state<"chat" | "settings" | "memory" | "graph">("chat");
  let fishFloating = $state(false); // companion fish is out on the desktop

  // Constellation view: the REAL cognee knowledge graph, distilled server-side (top entities
  // by tie degree). Layout is a deterministic golden-angle spiral — hubs center, quiet nodes
  // outer rings — so it reads like a galaxy without a physics sim.
  type GNode = { label: string; cat: string; deg: number; desc: string };
  type GView = { nodes: GNode[]; edges: [number, number][]; cats: [string, number][]; total_entities: number; total_ties: number };
  const CAT_COLORS: Record<string, string> = {
    People: "#ffb26b", Orgs: "#ff7a9c", Projects: "#59d6b5", Tools: "#6bb7ff", Topics: "#b58cff", "": "#75777f",
  };
  let gview = $state<GView | null>(null);
  let gloading = $state(false);
  let activeCat = $state<string | null>(null); // tappable chips: isolate one category
  let selected = $state<(GNode & { idx: number }) | null>(null); // click → detail card
  let hoverIdx = -1;
  let hoverNbrs = new Set<number>(); // direct neighbors of the hovered node (spotlight)
  let gcanvas: HTMLCanvasElement | null = null;
  let gpos: { x: number; y: number }[] = [];

  // Memory view: engine vitals + a search over the SAME recall path the drafts use.
  type MStatus = { sidecar_ok: boolean; datasets: string[]; local_snapshots: number };
  type Fact = { text: string; dataset: string | null };
  let mstatus = $state<MStatus | null>(null);
  let memQ = $state("");
  let memResults = $state<Fact[]>([]);
  let memBusy = $state(false);
  let memSearched = $state(false);

  type Seg = { app_bundle: string; start_ts: number; end_ts: number; count: number };
  type Msg = { role: "user" | "assistant"; text: string };
  let messages = $state<Msg[]>([]);
  let input = $state("");
  // Chat starters — generated from your memory (cognee graph) on the backend; these are the fallbacks.
  let starters = $state<string[]>([
    "What did I work on today?",
    "Who did I talk to recently?",
    "What's on my plate this week?",
  ]);
  let thinking = $state(false);

  let name = $state("");
  let paused = $state(false);
  let pulse = $state(false); // brief flash on the status dot when a snapshot is captured
  let exclusions = $state<string[]>([]);
  let newExcl = $state("");
  let today = $state<Seg[]>([]);

  // Native dock (Rust): sets the NSWindow frame's top-left at the very top of the screen,
  // centered — so it sits IN the menu-bar strip. The framework's setPosition
  // clamps below the menu bar; setFrameTopLeftPoint on a borderless panel doesn't.
  async function dock(w: number, h: number, duration = 0) {
    await invoke("dock_panel", { width: w, height: h, duration });
  }
  // The manage views (settings/profiles/memory/constellation/wiki) render inside the SAME panel
  // as chat — same size, same top dock. The gear just swaps content; a compact sidebar fits it.
  // Manage stays locked open (no collapse-on-leave) so sidebar navigation doesn't dismiss it.
  let bigMode = $derived(expanded && view !== "chat");

  async function loadData() {
    try {
      name = (await invoke<string | null>("get_user_name")) ?? "";
      paused = await invoke<boolean>("get_paused");
      exclusions = await invoke<string[]>("get_exclusions");
      today = await invoke<Seg[]>("chronicle_today");
      void refreshUnread(); // inbox badge reflects reality on every open
    } catch (e) {
      console.error("load failed", e);
    }
  }

  async function expand() {
    if (expanded) return; // guard: the monitor may emit enter repeatedly
    expanded = true;
    void invoke("set_panel_expanded", { expanded: true }); // monitor now hit-tests the panel
    await dock(PANEL_W, PANEL_H, 0.28); // calm downward open
    await loadData();
  }
  async function collapse() {
    if (!expanded) return;
    // 1. fade the content out quickly, while the window stays panel-sized
    closing = true;
    await new Promise((r) => setTimeout(r, 150));
    // 2. shrink the window — same duration as the open (panel bg only; content is invisible)
    void invoke("set_panel_expanded", { expanded: false }); // monitor back to the pill region
    await dock(PILL_W, PILL_H, 0.28);
    await new Promise((r) => setTimeout(r, 300)); // let the shrink animation finish
    // 3. swap to the pill (window is already small) and reset
    expanded = false;
    view = "chat";
    closing = false;
    inboxOpen = false;
    if (memTimer) { clearInterval(memTimer); memTimer = null; }
  }

  // Hover to open; leave to close (after a short grace), unless an input inside is focused.
  let collapseTimer: ReturnType<typeof setTimeout> | null = null;
  let locked = false;
  function scheduleCollapse() {
    if (collapseTimer) clearTimeout(collapseTimer);
    collapseTimer = setTimeout(() => {
      if (!locked && !bigMode) void collapse(); // the manage window is locked (click/esc-to-close)
    }, 350);
  }
  function cancelCollapse() {
    if (collapseTimer) clearTimeout(collapseTimer);
  }

  // Click the dock slot to pop the fish onto the desktop; click again (empty slot) to recall.
  async function toggleFish() {
    if (fishFloating) await invoke("fish_dock");
    else await invoke("fish_undock");
  }

  onMount(() => {
    void dock(PILL_W, PILL_H, 0); // instant on first paint
    setTimeout(() => void dock(PILL_W, PILL_H, 0), 300); // re-center once the monitor is ready
    // Reflect REAL capture state on the pill dot from launch (not the default-false).
    void invoke<boolean>("get_paused").then((p) => (paused = p));
    // Dynamic starters from your memory (cognee graph) — falls back to the defaults on error.
    void invoke<string[]>("chat_starters").then((s) => { if (s?.length) starters = s; }).catch(() => {});
    // Hover-to-open from the focus-independent global cursor monitor (lib.rs).
    const un = listen<boolean>("quill://hover", (e) => {
      if (e.payload) {
        cancelCollapse(); // re-entering cancels a pending close
        void expand();
      } else {
        scheduleCollapse();
      }
    });
    // Keep the dock slot in sync with the floating fish (Rust is the source of truth).
    const unfish = listen<boolean>("quill://fish", (e) => {
      fishFloating = e.payload;
    });
    // Heartbeat: pulse the status dot when a snapshot is actually captured (real activity).
    const uncap = listen("quill://capture", () => {
      pulse = true;
      setTimeout(() => (pulse = false), 600);
    });
    return () => {
      void un.then((f) => f());
      void unfish.then((f) => f());
      void uncap.then((f) => f());
    };
  });

  // Minimal, dependency-free markdown → HTML for chat answers. Escapes HTML FIRST (XSS-safe),
  // then applies bold / inline-code / headings / bullets. The LLM replies in markdown; without
  // this the panel showed literal ** and * .
  function mdToHtml(src: string): string {
    const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    const inline = (s: string) =>
      esc(s)
        .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
        .replace(/`([^`]+)`/g, "<code>$1</code>");
    let html = "";
    let inList = false;
    for (const raw of src.split(/\r?\n/)) {
      const line = raw.replace(/\s+$/, "");
      const bullet = line.match(/^\s*[-*]\s+(.*)$/);
      const heading = line.match(/^\s*#{1,6}\s+(.*)$/);
      if (bullet) {
        if (!inList) { html += "<ul>"; inList = true; }
        html += `<li>${inline(bullet[1])}</li>`;
        continue;
      }
      if (inList) { html += "</ul>"; inList = false; }
      if (heading) html += `<div class="mdh">${inline(heading[1])}</div>`;
      else if (line.trim() !== "") html += `<p>${inline(line)}</p>`;
    }
    if (inList) html += "</ul>";
    return html;
  }

  async function send(prompt?: string) {
    const q = (prompt ?? input).trim();
    if (!q || thinking) return;
    input = "";
    messages = [...messages, { role: "user", text: q }];
    thinking = true;
    try {
      // Retry once: the self-hosted LLM intermittently 500s under load (cognify/memify), which
      // would otherwise surface as a one-off error and look like the chat is "broken".
      let a = "";
      for (let attempt = 0; ; attempt++) {
        try { a = await invoke<string>("ask", { message: q }); break; }
        catch (e) {
          if (attempt >= 1) throw e;
          await new Promise((r) => setTimeout(r, 900));
        }
      }
      messages = [...messages, { role: "assistant", text: a }];
    } catch (e) {
      messages = [...messages, { role: "assistant", text: "⚠️ " + e + " — the model may be busy; try again." }];
    } finally {
      thinking = false;
    }
  }

  // Auto-refresh while the memory view is open: a one-shot probe mislabeled a busy sidecar as
  // "offline" (observed live — it answered 0.1s later). Refresh keeps the vitals honest and
  // shows the pending backlog draining in real time.
  let memTimer: ReturnType<typeof setInterval> | null = null;
  async function refreshStatus() {
    const s = await invoke<MStatus>("memory_status").catch(() => null);
    if (s) mstatus = s; // keep showing the last good reading through a busy blip
  }
  function showMemory() {
    view = "memory";
    if (memTimer) clearInterval(memTimer);
    void refreshStatus();
    memTimer = setInterval(() => void refreshStatus(), 5000);
  }
  async function openMemory() {
    if (view === "memory") {
      view = "chat";
      if (memTimer) { clearInterval(memTimer); memTimer = null; }
      return;
    }
    mstatus = null;
    showMemory();
  }

  // Profiles: one persona per platform = HOW you write here (learned voice + signature + memory
  // circle). WHO you are is global now (Identity screen), so there is no per-profile identity field.
  type Prof = {
    key: string; label: string; voice: string[]; voice_samples: number;
    voice_examples: string[]; learning: boolean; signature: string;
    circle: string; shares_with: string[]; class: string;
  };
  let profiles = $state<Prof[]>([]);
  let profLoaded = $state(false);
  // Per-profile UI state (keyed by profile key), kept out of the backend shape.
  let previews = $state<Record<string, string>>({});
  let previewBusy = $state<Record<string, boolean>>({});
  let showExamples = $state<Record<string, boolean>>({});
  // Pending voice/identity change proposals awaiting review (old → new).
  type Proposal = { id: number; kind: string; key: string; before: string; after: string; created_at: number };
  let proposals = $state<Proposal[]>([]);

  async function openProfiles() {
    view = view === "profiles" ? "chat" : "profiles";
    if (view !== "profiles") return;
    void loadProfilesData();
  }
  async function saveProfile(p: Prof) {
    await invoke("set_profile_signature", { key: p.key, signature: p.signature }).catch(() => {});
    await invoke("set_profile_circle", { key: p.key, circle: p.circle || "work" }).catch(() => {});
  }
  async function doPreview(p: Prof) {
    if (previewBusy[p.key]) return;
    previewBusy[p.key] = true;
    try { previews[p.key] = await invoke<string>("preview_voice", { key: p.key }); }
    catch { previews[p.key] = "(couldn't draft a preview — the model may be busy)"; }
    finally { previewBusy[p.key] = false; }
  }
  async function toggleLearning(p: Prof) {
    p.learning = !p.learning;
    await invoke("set_voice_learning", { key: p.key, on: p.learning }).catch(() => {});
  }
  async function resolveProposal(id: number, approve: boolean) {
    await invoke("resolve_proposal", { id, approve }).catch(() => {});
    proposals = await invoke<Proposal[]>("list_proposals").catch(() => []);
    await loadProfilesData();
    if (view === "settings") await loadSettingsPage();
  }

  let inboxOpen = $state(false);
  type InboxItem = { id: number; ts: number; kind: string; title: string; body: string; read: boolean };
  let inboxItems = $state<InboxItem[]>([]);
  let inboxQ = $state("");
  let unread = $state(0);
  let expandedEvent = $state<number>(-1);

  // Manage surface: the gear opens ONE large centered window (not a dropdown) whose left sidebar
  // holds EVERY section. Chat stays the compact panel; everything else lives in this big window.
  const NAV = [
    { id: "identity", label: "Identity" },
    { id: "privacy", label: "Privacy" },
    { id: "profiles", label: "Profiles" },
    { id: "memory", label: "Memory" },
    { id: "constellation", label: "Constellation" },
    { id: "wiki", label: "Wiki" },
    { id: "shortcuts", label: "Shortcuts" },
  ] as const;
  let lastNav = "identity";
  function isNavActive(id: string): boolean {
    if (id === "identity") return view === "settings" && setSection === "identity";
    if (id === "privacy") return view === "settings" && setSection === "privacy";
    if (id === "shortcuts") return view === "settings" && setSection === "shortcuts";
    if (id === "profiles") return view === "profiles";
    if (id === "memory") return view === "memory";
    if (id === "constellation") return view === "graph";
    if (id === "wiki") return view === "wiki";
    return false;
  }
  function startMemTimer() {
    if (memTimer) clearInterval(memTimer);
    void refreshStatus();
    memTimer = setInterval(() => void refreshStatus(), 5000);
  }
  async function loadProfilesData() {
    profLoaded = false;
    profiles = await invoke<Prof[]>("list_profiles").catch(() => []);
    proposals = await invoke<Proposal[]>("list_proposals").catch(() => []);
    profLoaded = true;
  }
  function pickNav(id: string) {
    inboxOpen = false;
    lastNav = id;
    if (id === "identity" || id === "privacy" || id === "shortcuts") {
      view = "settings"; setSection = id as "identity" | "privacy" | "shortcuts"; void loadSettingsPage();
    } else if (id === "profiles") {
      view = "profiles"; void loadProfilesData();
    } else if (id === "memory") {
      view = "memory"; mstatus = null; startMemTimer();
    } else if (id === "constellation") {
      view = "graph"; void loadGraphData();
    } else if (id === "wiki") {
      view = "wiki"; void loadWikiData();
    }
  }
  function openManage() {
    if (!expanded) return;
    pickNav(lastNav);
  }

  // Settings sub-sections shown in the manage body (the unified sidebar handles navigation).
  let setSection = $state<"identity" | "privacy" | "shortcuts">("identity");
  let identityMd = $state("");       // the rich dossier the user reads/edits
  let identityBlurb = $state("");    // the short blurb actually injected into drafts (transparency)
  let axOk = $state<boolean | null>(null);
  let wipeArmed = $state(false);
  let synthBusy = $state(false);
  let autostart = $state(false);
  async function loadSettingsPage() {
    identityMd = (await invoke<string | null>("get_identity").catch(() => null)) ?? "";
    identityBlurb = (await invoke<string | null>("get_identity_blurb").catch(() => null)) ?? "";
    proposals = await invoke<Proposal[]>("list_proposals").catch(() => []);
    axOk = await invoke<boolean>("ax_trusted").catch(() => null);
    autostart = await invoke<boolean>("get_autostart").catch(() => false);
    void refreshStatus(); // the Memory section reuses the engine vitals
  }
  async function toggleAutostart() {
    autostart = !autostart;
    await invoke("set_autostart", { on: autostart }).catch(() => {});
  }
  async function saveIdentity() {
    await invoke("set_identity", { profile: identityMd }).catch(() => {});
  }
  async function resynthIdentity() {
    if (synthBusy) return;
    synthBusy = true;
    try {
      const draft = await invoke<string>("rebuild_identity");
      if (draft) identityMd = draft; // user reviews, then Save
    } catch { /* thin memory — keep the current text */ }
    finally { synthBusy = false; }
  }
  async function doWipe() {
    if (!wipeArmed) { wipeArmed = true; setTimeout(() => (wipeArmed = false), 6000); return; }
    wipeArmed = false;
    await invoke("wipe_all_data").catch(() => {});
    wiki = []; mstatus = null; gview = null; // stale views drop their data
    void refreshStatus();
    void refreshUnread();
  }
  async function refreshUnread() {
    unread = await invoke<number>("inbox_unread").catch(() => 0);
  }
  async function toggleInbox() {
    inboxOpen = !inboxOpen;
    if (inboxOpen) {
      inboxItems = await invoke<InboxItem[]>("list_inbox").catch(() => []);
      void refreshUnread();
    }
  }
  async function openEvent(it: InboxItem) {
    expandedEvent = expandedEvent === it.id ? -1 : it.id;
    if (!it.read) {
      it.read = true;
      unread = Math.max(0, unread - 1);
      void invoke("mark_inbox_read", { id: it.id }).catch(() => {});
    }
  }
  async function markAllRead() {
    inboxItems = inboxItems.map((i) => ({ ...i, read: true }));
    unread = 0;
    void invoke("mark_inbox_read", { id: null }).catch(() => {});
  }
  const inboxFiltered = $derived(
    inboxQ.trim()
      ? inboxItems.filter((i) => (i.title + " " + i.body).toLowerCase().includes(inboxQ.toLowerCase()))
      : inboxItems
  );
  async function newChat() {
    if (messages.length > 0) {
      const first = messages.find((m) => m.role === "user")?.text ?? "chat";
      const title = first.length > 60 ? first.slice(0, 60) + "…" : first;
      const transcript = messages.map((m) => `${m.role === "user" ? "you" : "quill"}: ${m.text}`).join("\n");
      void invoke("archive_chat", { title, transcript }).catch(() => {});
    }
    messages = [];
    input = "";
    view = "chat";
    inboxOpen = false;
  }

  // Memory wiki: a distilled page per entity, summarized from your captures.
  type WikiPage = { slug: string; title: string; kind: string; summary: string; mention_count: number; last_seen: number | null };
  let wiki = $state<WikiPage[]>([]);
  let wikiLoaded = $state(false);
  let wikiBusy = $state(false);
  async function loadWikiData() {
    wikiLoaded = false;
    wiki = await invoke<WikiPage[]>("list_wiki").catch(() => []);
    wikiLoaded = true;
  }
  async function distillWiki() {
    if (wikiBusy) return;
    wikiBusy = true;
    try {
      await invoke<number>("refresh_wiki");
      wiki = await invoke<WikiPage[]>("list_wiki").catch(() => wiki);
    } finally {
      wikiBusy = false;
    }
  }
  function relTime(secs: number | null): string {
    if (!secs) return "";
    const d = Math.max(0, Math.floor(Date.now() / 1000 - secs));
    if (d < 3600) return `${Math.floor(d / 60)}m ago`;
    if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
    return `${Math.floor(d / 86400)}d ago`;
  }

  async function loadGraphData() {
    if (!gview) {
      gloading = true;
      gview = await invoke<GView>("graph_view").catch(() => null);
      gloading = false;
    }
    // Longer delay than a normal redraw: on first open the window is still animating to its
    // large size, so we wait for the resize to settle before measuring the canvas.
    setTimeout(() => { seedLayout(); runForces(); renderGraph(); }, 340);
  }

  // Constellation structure: a dense force-clustered CORE (top entities by rank — where the
  // labels and edges live) surrounded by neat concentric RINGS of small dots. The core encodes
  // similarity (tie springs); the rings are deterministic and calm.
  const CORE = 72;
  function seedLayout() {
    if (!gview || !gcanvas) return;
    const w = gcanvas.clientWidth, h = gcanvas.clientHeight;
    const cx = w / 2, cy = h / 2;
    const rMax = Math.min(w, h) / 2 - 12;
    const GA = 2.399963;
    const n = gview.nodes.length;
    gpos = [];
    const nCore = Math.min(CORE, n);
    // Core: tight golden-angle spiral (force pass refines it below).
    const coreR = rMax * 0.42;
    for (let i = 0; i < nCore; i++) {
      const r = 10 + Math.pow(i / Math.max(1, nCore - 1), 0.7) * (coreR - 10);
      gpos.push({ x: cx + r * Math.cos(i * GA), y: cy + r * Math.sin(i * GA) });
    }
    // Periphery: evenly spaced rings, higher-ranked nodes on the inner ring; tiny deterministic
    // jitter keeps it organic without breaking the circle.
    const rest = n - nCore;
    const ringFractions = [0.64, 0.79, 0.94];
    const per = Math.ceil(rest / ringFractions.length);
    for (let j = 0; j < rest; j++) {
      const ring = Math.min(ringFractions.length - 1, Math.floor(j / per));
      const idxInRing = j - ring * per;
      const count = Math.min(per, rest - ring * per);
      const jitterA = ((((j * 2654435761) >>> 0) % 100) / 100 - 0.5) * 0.05;
      const jitterR = ((((j * 40503) >>> 0) % 100) / 100 - 0.5) * 7;
      const a = (idxInRing / Math.max(1, count)) * Math.PI * 2 + ring * 0.35 + jitterA;
      const r = rMax * ringFractions[ring] + jitterR;
      gpos.push({ x: cx + r * Math.cos(a), y: cy + r * Math.sin(a) });
    }
  }

  // Small force simulation, run ONCE per open: tie springs pull related nodes together,
  // repulsion keeps dots legible, gravity holds the shape. Two hard-won rules (observed live:
  // hub spring-forces exploded and the rectangular clamp pinned nodes into the corners — the
  // galaxy became a box): per-step displacement is capped by a cooling temperature, and the
  // boundary is a CIRCLE — overshooting nodes are pulled back onto the rim, never a corner.
  function runForces() {
    if (!gview || !gcanvas) return;
    // Force pass runs over the CORE only — the rings are deterministic and stay put.
    const n = Math.min(CORE, gview.nodes.length);
    const w = gcanvas.clientWidth, h = gcanvas.clientHeight;
    const cx = w / 2, cy = h / 2;
    const coreR = (Math.min(w, h) / 2 - 12) * 0.46;
    const fx = new Float32Array(n), fy = new Float32Array(n);
    const K_REP = 700, CUT2 = 70 * 70, REST = 26, K_SPR = 0.015, GRAV = 0.03;
    let temp = 7; // max px moved per iteration, cooling toward stillness
    for (let it = 0; it < 130; it++) {
      fx.fill(0); fy.fill(0);
      for (let i = 0; i < n; i++) {
        for (let j = i + 1; j < n; j++) {
          const dx = gpos[i].x - gpos[j].x, dy = gpos[i].y - gpos[j].y;
          const d2 = dx * dx + dy * dy + 0.01;
          if (d2 > CUT2) continue;
          const f = K_REP / d2;
          const d = Math.sqrt(d2);
          fx[i] += (dx / d) * f; fy[i] += (dy / d) * f;
          fx[j] -= (dx / d) * f; fy[j] -= (dy / d) * f;
        }
      }
      for (const [a, b] of gview.edges) {
        if (a >= n || b >= n) continue; // ring nodes don't participate
        const dx = gpos[b].x - gpos[a].x, dy = gpos[b].y - gpos[a].y;
        const d = Math.sqrt(dx * dx + dy * dy) + 0.01;
        // Normalize by the busier endpoint so 100-tie hubs don't accumulate explosive pull.
        const norm = 1 + Math.sqrt(Math.min(gview.nodes[a].deg, gview.nodes[b].deg));
        const f = ((d - REST) * K_SPR) / norm;
        fx[a] += (dx / d) * f; fy[a] += (dy / d) * f;
        fx[b] -= (dx / d) * f; fy[b] -= (dy / d) * f;
      }
      for (let i = 0; i < n; i++) {
        fx[i] += (cx - gpos[i].x) * GRAV;
        fy[i] += (cy - gpos[i].y) * GRAV;
        // Temperature-capped step (Fruchterman–Reingold style) instead of velocity: bounded,
        // cooling motion that cannot explode.
        const m = Math.sqrt(fx[i] * fx[i] + fy[i] * fy[i]) + 1e-6;
        const step = Math.min(m, temp);
        let x = gpos[i].x + (fx[i] / m) * step;
        let y = gpos[i].y + (fy[i] / m) * step;
        // Keep the core INSIDE its disc so it never collides with the rings.
        const ddx = x - cx, ddy = y - cy;
        const r = Math.sqrt(ddx * ddx + ddy * ddy);
        if (r > coreR) {
          x = cx + (ddx / r) * coreR;
          y = cy + (ddy / r) * coreR;
        }
        gpos[i].x = x;
        gpos[i].y = y;
      }
      temp = Math.max(1, temp * 0.975);
    }
  }

  function renderGraph() {
    if (!gview || !gcanvas) return;
    const dpr = window.devicePixelRatio || 1;
    const w = gcanvas.clientWidth, h = gcanvas.clientHeight;
    gcanvas.width = w * dpr;
    gcanvas.height = h * dpr;
    const ctx = gcanvas.getContext("2d")!;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, w, h);
    const focus = activeCat;
    const inFocus = (i: number) => !focus || gview!.nodes[i].cat === focus;
    // Spotlight: while hovering, only the node + its direct neighbors stay
    // lit; the category filter governs when nothing is hovered.
    const spot = hoverIdx >= 0;
    const lit = (i: number) => (spot ? i === hoverIdx || hoverNbrs.has(i) : inFocus(i));
    ctx.lineWidth = 1;
    for (const [a, b] of gview.edges) {
      const p = gpos[a], q = gpos[b];
      if (!p || !q) continue;
      // Calm-state edges live in the CORE only (the rings stay clean);
      // hovering a node lights ITS ties wherever they reach.
      const hoverEdge = spot && (a === hoverIdx || b === hoverIdx);
      if (!hoverEdge && (a >= CORE || b >= CORE)) continue;
      const on = spot ? hoverEdge : inFocus(a) && inFocus(b);
      ctx.strokeStyle = on ? "rgba(255,255,255,0.15)" : "rgba(255,255,255,0.035)";
      ctx.beginPath(); ctx.moveTo(p.x, p.y); ctx.lineTo(q.x, q.y); ctx.stroke();
    }
    gview.nodes.forEach((n, i) => {
      const p = gpos[i];
      const on = lit(i);
      // Small, near-uniform dots: rings 2.4, core 3.1, top hubs 4.2.
      const base = i < 12 ? 4.2 : i < CORE ? 3.1 : 2.4;
      const r = base * (on ? 1 : 0.78);
      ctx.globalAlpha = on ? 0.95 : spot ? 0.13 : 0.3;
      ctx.fillStyle = CAT_COLORS[n.cat] ?? CAT_COLORS[""];
      ctx.shadowBlur = on && (spot ? i === hoverIdx : i < 12) ? 7 : 0;
      ctx.shadowColor = ctx.fillStyle;
      ctx.beginPath();
      ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
      ctx.fill();
    });
    ctx.globalAlpha = 1;
    ctx.shadowBlur = 0;
    // Selected node keeps a ring while its detail card is open.
    const ringIdx = spot ? hoverIdx : (selected?.idx ?? -1);
    if (ringIdx >= 0 && gpos[ringIdx]) {
      const p = gpos[ringIdx];
      ctx.strokeStyle = "#ffb26b";
      ctx.lineWidth = 1.6;
      ctx.beginPath();
      ctx.arc(p.x, p.y, 9, 0, Math.PI * 2);
      ctx.stroke();
    }
    ctx.font = "600 11px -apple-system, system-ui";
    ctx.fillStyle = "rgba(255,255,255,0.95)";
    ctx.textAlign = "center";
    ctx.shadowColor = "rgba(0,0,0,0.85)"; ctx.shadowBlur = 3; // halo so labels read over dots/edges
    if (spot) {
      // Inline label next to the spotlit dot — the spotlight interaction.
      const n = gview.nodes[hoverIdx], p = gpos[hoverIdx];
      ctx.fillText(n.label.length > 28 ? n.label.slice(0, 28) + "…" : n.label, p.x, p.y - 13);
    } else {
      // Label the most-connected nodes, but SKIP any that would collide with a label already
      // drawn — so the crowded core stops turning into a pile of overlapping text. More labels
      // then appear out in the sparse rings, where there's room.
      const drawn: { x1: number; y1: number; x2: number; y2: number }[] = [];
      let labeled = 0;
      for (let i = 0; i < gview.nodes.length && labeled < 18; i++) {
        if (!inFocus(i)) continue;
        const n = gview.nodes[i], p = gpos[i];
        const txt = n.label.length > 22 ? n.label.slice(0, 22) + "…" : n.label;
        const w = ctx.measureText(txt).width;
        const box = { x1: p.x - w / 2 - 3, y1: p.y - 21, x2: p.x + w / 2 + 3, y2: p.y - 5 };
        if (drawn.some((d) => box.x1 < d.x2 && box.x2 > d.x1 && box.y1 < d.y2 && box.y2 > d.y1)) continue;
        ctx.fillText(txt, p.x, p.y - 9);
        drawn.push(box);
        labeled++;
      }
    }
    ctx.shadowBlur = 0;
  }

  function toggleCat(c: string) {
    // Selecting a tag is a full context switch: previous tag, open card and hover all reset.
    activeCat = activeCat === c ? null : c;
    selected = null;
    hoverIdx = -1;
    hoverNbrs = new Set();
    renderGraph();
  }

  function nearestNode(e: MouseEvent): number {
    if (!gcanvas) return -1;
    const rect = gcanvas.getBoundingClientRect();
    const x = e.clientX - rect.left, y = e.clientY - rect.top;
    let best = -1, bd = 324; // 18px pick radius — dots are small, targets must not be
    gpos.forEach((p, i) => {
      // Dimmed dots are INERT while a tag filter is active — clear the filter to reach them.
      if (activeCat && gview && gview.nodes[i].cat !== activeCat) return;
      const d = (p.x - x) ** 2 + (p.y - y) ** 2;
      if (d < bd) { bd = d; best = i; }
    });
    return best;
  }
  function graphHover(e: MouseEvent) {
    const i = nearestNode(e);
    if (i === hoverIdx) return;
    hoverIdx = i;
    hoverNbrs = new Set();
    if (i >= 0 && gview) {
      for (const [a, b] of gview.edges) {
        if (a === i) hoverNbrs.add(b);
        else if (b === i) hoverNbrs.add(a);
      }
    }
    if (gcanvas) gcanvas.style.cursor = i >= 0 ? "pointer" : "default";
    renderGraph();
  }
  // Click → detail card (name · category · ties + cognify's own description). The card's
  // "Recall" button then queries this entity through the SAME retrieval path the drafts use.
  // Empty-canvas clicks dismiss in layers: open card first, then the active tag filter.
  function graphClick(e: MouseEvent) {
    const i = nearestNode(e);
    if (i >= 0 && gview) {
      selected = { ...gview.nodes[i], idx: i };
    } else if (selected) {
      selected = null;
    } else if (activeCat) {
      activeCat = null;
    }
    renderGraph();
  }
  function recallSelected() {
    if (!selected) return;
    memQ = selected.label;
    selected = null;
    showMemory();
    void searchMem();
  }
  async function searchMem() {
    const q = memQ.trim();
    if (!q || memBusy) return;
    memBusy = true;
    memSearched = true;
    try {
      memResults = await invoke<Fact[]>("search_memory", { query: q });
    } catch {
      memResults = [];
    } finally {
      memBusy = false;
    }
  }

  async function saveName() { await invoke("set_user_name", { name }); }
  async function togglePause() { paused = !paused; await invoke("set_paused", { paused }); }
  async function addExcl() {
    const v = newExcl.trim();
    if (!v || exclusions.includes(v)) return;
    exclusions = [...exclusions, v]; newExcl = "";
    await invoke("set_exclusions", { list: exclusions });
  }
  async function removeExcl(i: number) {
    exclusions = exclusions.filter((_, x) => x !== i);
    await invoke("set_exclusions", { list: exclusions });
  }

  const appName = (b: string) => b.split(".").pop() ?? b;
  const firstName = $derived(name.trim().split(/\s+/)[0] ?? "");
</script>

{#if !expanded}
  <button class="pill" aria-label="quill">
    <span class="dot" class:on={!paused} class:pulse></span>
    <span class="brand">quill</span>
  </button>
{:else}
  <div
    class="panel"
    onmouseenter={cancelCollapse}
    onmouseleave={scheduleCollapse}
    role="dialog"
    tabindex="-1"
  >
    <div class="inner" class:closing in:fade={{ delay: 280, duration: 220 }}>
    <div class="toolbar">
      <button class="ic fish" class:empty={fishFloating} onclick={toggleFish} aria-label="fish — send to desktop / recall">
        {#if !fishFloating}
          <svg viewBox="40 25 110 70"><ellipse cx="100" cy="60" rx="38" ry="28" fill="#ff7a2e" /><path d="M74 60 C 50 40, 34 46, 32 60 C 34 74, 50 80, 74 60 Z" fill="#ff5814" /><circle cx="122" cy="54" r="6" fill="#fff" /><circle cx="124" cy="54" r="3" fill="#1b1b1b" /></svg>
        {/if}
      </button>
      <button class="ic" class:active={view !== "chat" && view !== "graph"} onclick={openManage} aria-label="settings">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3.1" /><path d="M19.4 13a7.8 7.8 0 000-2l2.1-1.6-2-3.5-2.5 1a7.8 7.8 0 00-1.7-1L14 2.2h-4l-.4 2.7a7.8 7.8 0 00-1.7 1l-2.5-1-2 3.5L5.5 11a7.8 7.8 0 000 2l-2.1 1.6 2 3.5 2.5-1a7.8 7.8 0 001.7 1l.4 2.7h4l.4-2.7a7.8 7.8 0 001.7-1l2.5 1 2-3.5z" /></svg>
      </button>
      <button class="ic" class:active={view === "graph"} onclick={() => pickNav("constellation")} aria-label="constellation">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="2.4" /><ellipse cx="12" cy="12" rx="9" ry="4" /><ellipse cx="12" cy="12" rx="9" ry="4" transform="rotate(62 12 12)" /></svg>
      </button>
      <span class="ring" class:on={!paused} class:pulse></span>
      <span class="spacer"></span>
      <button class="ic inboxbtn" class:active={inboxOpen} onclick={toggleInbox} aria-label="inbox">
        <svg viewBox="0 0 24 24"><path d="M3 12h5l1 2h6l1-2h5M4 12V6a2 2 0 012-2h12a2 2 0 012 2v6M4 12v4a2 2 0 002 2h12a2 2 0 002-2v-4" /></svg>
        {#if unread > 0}<span class="badge">{unread > 9 ? "9+" : unread}</span>{/if}
      </button>
      <button class="ic" onclick={newChat} aria-label="new chat"><svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14" /></svg></button>
      <button class="esc" onclick={collapse}>esc</button>
    </div>

    {#if inboxOpen}
      <div class="drop inbox" transition:fade={{ duration: 100 }}>
        <div class="inboxbar">
          <svg viewBox="0 0 24 24" class="searchic"><circle cx="11" cy="11" r="7" /><path d="M21 21l-4.3-4.3" /></svg>
          <input bind:value={inboxQ} placeholder="Search inbox…"
            onfocus={() => (locked = true)} onblur={() => (locked = false)} />
          <button class="markread" onclick={markAllRead}>Mark read</button>
        </div>
        <div class="inboxlist">
          {#if inboxFiltered.length === 0}
            <p class="hint" style="padding: 10px 14px;">nothing here yet — drafts, memory digests and system notices land in this inbox.</p>
          {/if}
          {#each inboxFiltered as it (it.id)}
            <button class="inrow" class:unread={!it.read} onclick={() => openEvent(it)}>
              <span class="indot" class:on={!it.read}></span>
              <span class="intitle">{it.title}</span>
              <span class="intime">{relTime(it.ts)}</span>
            </button>
            {#if expandedEvent === it.id && it.body}
              <div class="inbody">{it.body}</div>
            {/if}
          {/each}
        </div>
      </div>
    {/if}

    {#if view === "chat"}
      <div class="content">
        {#if messages.length === 0}
          <div class="bubble assistant">
            hey{firstName ? " " + firstName : ""} 👋 — I remember what you've been working on. Ask me anything, or try:
          </div>
          <div class="starters">
            {#each starters as s}
              <button class="starter" onclick={() => send(s)}>{s}</button>
            {/each}
          </div>
        {/if}
        {#each messages as m}
          <div class="bubble {m.role}">
            {#if m.role === "assistant"}{@html mdToHtml(m.text)}{:else}{m.text}{/if}
          </div>
        {/each}
        {#if thinking}<div class="bubble assistant thinking">Quilling…</div>{/if}
      </div>
      <div class="composer">
        <input
          bind:value={input}
          placeholder="Message quill…"
          onfocus={() => (locked = true)}
          onblur={() => (locked = false)}
          onkeydown={(e) => e.key === "Enter" && send()}
        />
        <button class="send" onclick={() => send()} aria-label="send"><svg viewBox="0 0 24 24"><path d="M12 19V5M5 12l7-7 7 7" /></svg></button>
      </div>
    {:else}
      <div class="bigrow">
        <nav class="setnav managenav">
          <h1>quill</h1>
          {#each NAV as n}
            <button class:sel={isNavActive(n.id)} onclick={() => pickNav(n.id)}>{n.label}</button>
          {/each}
        </nav>
        <div class="bigbody" class:graphfull={view === "graph"}>
          {#if view === "wiki"}
      <div class="content settings">
        <section>
          <h2>Memory wiki</h2>
          <p class="hint">A distilled page per person, project & tool — summarized from your own captures. Fills in while you're idle.</p>
          <button class="primary gsmall" onclick={distillWiki} disabled={wikiBusy}>{wikiBusy ? "Distilling…" : "Distill now"}</button>
        </section>
        {#if !wikiLoaded}
          <p class="hint">loading…</p>
        {:else if wiki.length === 0}
          <p class="hint">No pages yet — click "Distill now", or let it run while you're away.</p>
        {:else}
          {#each wiki as w}
            <section>
              <h2>{w.title} <span class="pclass">{w.kind}</span></h2>
              <p class="wsum">{w.summary}</p>
              <p class="hint">{w.mention_count} mention{w.mention_count === 1 ? "" : "s"}{w.last_seen ? " · last seen " + relTime(w.last_seen) : ""}</p>
            </section>
          {/each}
        {/if}
      </div>
    {:else if view === "profiles"}
      <div class="content settings">
        <section>
          <h2>Profiles</h2>
          <p class="hint">Each platform is <em>how</em> you write there. Quill learns your voice from your own messages — <em>who</em> you are lives in Identity.</p>
        </section>

        {#each proposals.filter((x) => x.kind === "voice") as pr}
          <section class="review">
            <h2>Voice update ready <span class="pclass">{profiles.find((p) => p.key === pr.key)?.label ?? pr.key}</span></h2>
            <p class="hint">Quill relearned your voice here. Approve to update it, or dismiss to keep the current one.</p>
            <div class="diff">
              <div class="diffcol"><span class="difflabel">current</span><pre>{pr.before}</pre></div>
              <div class="diffcol"><span class="difflabel dnew">proposed</span><pre>{pr.after}</pre></div>
            </div>
            <div class="row">
              <button class="primary gsmall" onclick={() => resolveProposal(pr.id, true)}>Approve</button>
              <button class="ghost gsmall" onclick={() => resolveProposal(pr.id, false)}>Dismiss</button>
            </div>
          </section>
        {/each}

        {#if !profLoaded}
          <p class="hint">loading…</p>
        {:else if profiles.length === 0}
          <p class="hint">No profiles yet — write on a platform (LinkedIn, Outlook, Teams…) and Quill learns it.</p>
        {:else}
          {#each profiles as p}
            <section>
              <h2>{p.label} <span class="pclass">{p.class}</span></h2>
              {#if p.voice.length}
                <ul class="voicelist">{#each p.voice as b}<li>{b}</li>{/each}</ul>
                <p class="hint">
                  learned from {p.voice_samples} message{p.voice_samples === 1 ? "" : "s"} you wrote here{#if p.voice_examples.length} · <button class="linkbtn" onclick={() => (showExamples[p.key] = !showExamples[p.key])}>{showExamples[p.key] ? "hide examples" : "see examples"}</button>{/if}
                </p>
                {#if showExamples[p.key]}
                  <ul class="examples">{#each p.voice_examples as ex}<li>“{ex}”</li>{/each}</ul>
                {/if}
              {:else}
                <p class="hint">learning your voice — {p.voice_samples} sample{p.voice_samples === 1 ? "" : "s"} so far</p>
              {/if}

              <div class="row prow">
                <button class="ghost gsmall" onclick={() => doPreview(p)} disabled={previewBusy[p.key] || !p.voice.length}>
                  {previewBusy[p.key] ? "Drafting…" : "Preview voice →"}
                </button>
                <label class="toggle small"><input type="checkbox" checked={p.learning} onchange={() => toggleLearning(p)} />Learn my voice here</label>
              </div>
              {#if previews[p.key]}
                <div class="preview"><span class="difflabel">sample draft in your voice</span><p>{previews[p.key]}</p></div>
              {/if}

              <textarea
                class="pfield" rows="2" bind:value={p.signature}
                placeholder="Sign-off for this platform (e.g. Best regards,&#10;Jordan Rivera) — blank = casual default"
                onfocus={() => (locked = true)} onblur={() => (locked = false)}
              ></textarea>
              <label class="circlerow">Memory circle
                <input class="circinput" bind:value={p.circle} placeholder="work"
                  onfocus={() => (locked = true)} onblur={() => (locked = false)} />
              </label>
              <p class="hint">
                Same circle name = shared memory. {#if p.shares_with.length}Shares memory with {p.shares_with.join(", ")}.{:else}Walled off — this circle isn't shared with any other platform.{/if}
              </p>
              <button class="primary gsmall" onclick={() => saveProfile(p)}>Save {p.label}</button>
            </section>
          {/each}
        {/if}
      </div>
    {:else if view === "graph"}
      <div class="content graphview">
        {#if gloading}
          <p class="hint gcenter">mapping your memory graph…</p>
        {:else if gview}
          <div class="chips gchips">
            {#each gview.cats as [c, n]}
              <button class="chip gchip" class:sel={activeCat === c} onclick={() => toggleCat(c)}>
                <span class="cdot" style="background:{CAT_COLORS[c]}"></span>{c}&nbsp;<span class="cnt">{n}</span>
              </button>
            {/each}
          </div>
          <div class="gwrap">
            <canvas
              bind:this={gcanvas}
              onmousemove={graphHover}
              onmouseleave={() => { hoverIdx = -1; hoverNbrs = new Set(); renderGraph(); }}
              onclick={graphClick}
            ></canvas>
            {#if selected}
              <div class="gcard" transition:fade={{ duration: 120 }}>
                <div class="gcard-head">
                  <span class="cdot" style="background:{CAT_COLORS[selected.cat]}"></span>
                  <strong>{selected.label}</strong>
                  <span class="d">{selected.cat || "untyped"} · {selected.deg} tie{selected.deg === 1 ? "" : "s"}</span>
                  <span class="spacer"></span>
                  <button class="x" onclick={() => { selected = null; renderGraph(); }}>×</button>
                </div>
                {#if selected.desc}<p>{selected.desc}</p>{/if}
                <button class="primary gsmall" onclick={recallSelected}>Recall from memory →</button>
              </div>
            {/if}
          </div>
          <div class="gfoot">
            showing top {gview.nodes.length} of {gview.total_entities.toLocaleString()} entities · {gview.total_ties.toLocaleString()} ties
            <span class="d">— the most-connected slice of your memory (click a dot)</span>
          </div>
        {:else}
          <p class="hint gcenter">graph unavailable — is the memory sidecar up?</p>
        {/if}
      </div>
    {:else if view === "memory"}
      <div class="content settings">
        <section>
          <h2>Memory engine</h2>
          {#if mstatus}
            <div class="statusline">
              <span class="dot" class:on={mstatus.sidecar_ok}></span>
              <span>{mstatus.sidecar_ok ? "cognee online" : "cognee busy — recall falls back to local memory"}</span>
              {#if mstatus.datasets.length}<span class="d">· {mstatus.datasets.length} graph datasets</span>{/if}
            </div>
            <p class="hint">
              {mstatus.local_snapshots.toLocaleString()} snapshots in local memory · instant recall, always available
            </p>
            {#if mstatus.datasets.length}
              <div class="chips">
                {#each mstatus.datasets.slice(0, 10) as ds}<span class="chip">{ds}</span>{/each}
                {#if mstatus.datasets.length > 10}<span class="empty">+{mstatus.datasets.length - 10} more</span>{/if}
              </div>
            {/if}
          {:else}
            <p class="hint">checking the sidecar…</p>
          {/if}
        </section>
        <section>
          <h2>Search your memory</h2>
          <p class="hint">The same recall your drafts use — excerpts straight from the graph.</p>
          <div class="row">
            <input
              bind:value={memQ}
              placeholder="what did we decide about…"
              onfocus={() => (locked = true)}
              onblur={() => (locked = false)}
              onkeydown={(e) => e.key === "Enter" && searchMem()}
            />
            <button class="primary" onclick={searchMem}>Recall</button>
          </div>
          {#if memBusy}<p class="hint">recalling…</p>{/if}
          {#each memResults as r}
            <div class="fact">
              {#if r.dataset}<span class="chip src">{r.dataset}</span>{/if}
              <p>{r.text.length > 280 ? r.text.slice(0, 280) + "…" : r.text}</p>
            </div>
          {/each}
          {#if memSearched && !memBusy && memResults.length === 0}
            <p class="hint">nothing relevant in the graph yet.</p>
          {/if}
        </section>
      </div>
    {:else}
      <div class="content settings setcards">
          {#if setSection === "identity"}
            <div class="setcard">
              <h3>Name</h3>
              <p class="hint">Who quill writes <em>as</em> — keeps replies role-correct.</p>
              <div class="row"><input bind:value={name} placeholder="Your name" onfocus={() => (locked = true)} onblur={() => (locked = false)} /><button class="primary" onclick={saveName}>Save</button></div>
            </div>
            {#each proposals.filter((x) => x.kind === "identity") as pr}
              <div class="setcard review">
                <h3>Identity update ready</h3>
                <p class="hint">Quill refreshed your profile from recent activity. Approve to update it, or dismiss to keep the current one.</p>
                <div class="diff">
                  <div class="diffcol"><span class="difflabel">current</span><pre>{pr.before}</pre></div>
                  <div class="diffcol"><span class="difflabel dnew">proposed</span><pre>{pr.after}</pre></div>
                </div>
                <div class="row">
                  <button class="primary gsmall" onclick={() => resolveProposal(pr.id, true)}>Approve</button>
                  <button class="ghost gsmall" onclick={() => resolveProposal(pr.id, false)}>Dismiss</button>
                </div>
              </div>
            {/each}
            <div class="setcard">
              <h3>Identity</h3>
              <p class="hint">A profile of your role, tools, habits &amp; voice — grounds every draft. Auto-built from your memory; edit freely. This is global; per-platform <em>voice</em> lives in Profiles.</p>
              <textarea class="pfield tall" rows="14" bind:value={identityMd} placeholder="Click “Re-synthesize from memory” to build this from what Quill has learned about you…" onfocus={() => (locked = true)} onblur={() => (locked = false)}></textarea>
              <div class="row" style="margin-top:8px">
                <button class="primary gsmall" onclick={saveIdentity}>Save</button>
                <button class="ghost gsmall" onclick={resynthIdentity} disabled={synthBusy}>{synthBusy ? "Synthesizing…" : "Re-synthesize from memory"}</button>
              </div>
              {#if identityBlurb.trim()}
                <details class="blurb">
                  <summary>What Quill tells the model about you (injected into every draft)</summary>
                  <pre>{identityBlurb}</pre>
                </details>
              {/if}
            </div>
          {:else if setSection === "privacy"}
            <div class="setcard">
              <h3 class="cardlabel">Permissions</h3>
              <p class="hint">What quill needs from macOS to build your memory.</p>
              <div class="permrow">
                <div>
                  <strong>Accessibility</strong>
                  <p class="hint">Required. Lets quill read on-screen text to build your memory.</p>
                </div>
                <span class="permstate" class:ok={axOk === true}>
                  <span class="dot" class:on={axOk === true}></span>{axOk === null ? "checking…" : axOk ? "Granted" : "Not granted"}
                </span>
              </div>
              {#if axOk === false}
                <button class="primary gsmall" onclick={() => invoke("open_ax_settings")}>Open System Settings</button>
              {/if}
            </div>
            <div class="setcard">
              <h3>Capture</h3>
              <label class="toggle"><input type="checkbox" checked={!paused} onchange={togglePause} />{paused ? "Paused" : "Capturing"}</label>
              <p class="hint">Reads your focused window to build local memory. Local-only; nothing leaves this Mac except your LLM endpoint.</p>
              <label class="toggle" style="margin-top:10px"><input type="checkbox" checked={autostart} onchange={toggleAutostart} />Launch at login</label>
              <p class="hint">Starts Quill automatically so your memory has no gaps. ⌘Q hides the panel — quit for real from the menu-bar fish → Quit Quill.</p>
              <h3 style="margin-top:10px">Excluded apps</h3>
              <p class="hint">Never captured — and excluding an app forgets its memory.</p>
              <div class="chips">
                {#each exclusions as ex, i}<span class="chip">{ex}<button class="x" onclick={() => removeExcl(i)}>×</button></span>{/each}
                {#if exclusions.length === 0}<span class="empty">none</span>{/if}
              </div>
              <div class="row"><input bind:value={newExcl} placeholder="com.hnc.Discord" onfocus={() => (locked = true)} onblur={() => (locked = false)} onkeydown={(e) => e.key === "Enter" && addExcl()} /><button class="primary" onclick={addExcl}>Add</button></div>
            </div>
            <div class="setcard danger">
              <h3 class="dangertitle">Danger zone</h3>
              <p class="hint">This action is irreversible. Wipes local memory — snapshots, wiki, learned style, inbox. Settings survive.</p>
              <button class="dangerbtn" onclick={doWipe}>{wipeArmed ? "Click again to really delete everything" : "🗑 Delete all data"}</button>
            </div>
          {:else}
            <div class="setcard">
              <h3>Shortcuts</h3>
              <div class="scrow"><span class="key">right ⌥</span><span>compose / rewrite in any text field</span></div>
              <div class="scrow"><span class="key">right ⌥ again</span><span>re-roll a different variant</span></div>
              <div class="scrow"><span class="key">CAPS INSTRUCTION</span><span>directed edit of the last draft ("MAKE IT SHORTER")</span></div>
              <div class="scrow"><span class="key">// instruction</span><span>execute against your memory instead of polishing</span></div>
              <div class="scrow"><span class="key">esc</span><span>close this panel</span></div>
            </div>
          {/if}
      </div>
      {/if}
        </div>
      </div>
    {/if}
    </div>
  </div>
{/if}

<style>
  :global(html), :global(body) { margin: 0; padding: 0; background: transparent !important; overflow: hidden; font: 13px/1.5 -apple-system, system-ui, sans-serif; color: #ededf0; }
  /* Warm palette — coral→gold over a warm ink base. RGB triplets so opacity can vary. */
  :global(:root) {
    --gf-orange: 255, 122, 46;  /* brand body */
    --gf-coral:  255, 88, 30;   /* deeper fin */
    --gf-gold:   255, 194, 120; /* pale-gold highlight / belly sheen */
    --gf-ink:    26, 17, 10;    /* warm near-black base */
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: #6b6d75; flex: none; }
  .dot.on { background: #ff7a2e; box-shadow: 0 0 7px #ff7a2eb0; }
  .dot.pulse, .ring.pulse { animation: statuspulse 0.6s ease; }
  @keyframes statuspulse { 0% { transform: scale(1); } 45% { transform: scale(1.85); } 100% { transform: scale(1); } }
  .ring { width: 13px; height: 13px; border-radius: 50%; border: 2px solid #555; flex: none; }
  .ring.on { border-color: #ff7a2e; box-shadow: 0 0 6px #ff7a2e90; }
  .brand { font-weight: 700; color: #ffddc0; letter-spacing: 0.01em; text-shadow: 0 0 12px rgba(var(--gf-orange), 0.45); }

  .pill {
    width: 100vw; height: 100vh; box-sizing: border-box; display: flex; align-items: center; justify-content: center;
    position: relative; padding: 0 16px; border: 0; cursor: pointer; color: #ededf0;
    /* Dark glass lit from BELOW: a warm amber pool along the bottom edge, an under-glow rising from
       it, and a soft warm bloom spilling out beneath — the "cool" lit-bar look. */
    background:
      radial-gradient(85% 150% at 50% 128%, rgba(var(--gf-orange), 0.5), rgba(var(--gf-coral), 0.14) 46%, transparent 66%),
      linear-gradient(180deg, rgba(var(--gf-ink), 0.66) 34%, rgba(var(--gf-coral), 0.10) 100%);
    -webkit-backdrop-filter: blur(42px) saturate(1.6);
    backdrop-filter: blur(42px) saturate(1.6);
    border-radius: 0 0 20px 20px;
    box-shadow:
      0 10px 30px rgba(0,0,0,0.5),
      0 7px 26px -8px rgba(var(--gf-orange), 0.5),           /* warm bloom spilling below */
      inset 0 1px 0 rgba(255,255,255,0.15),                  /* top edge catch */
      inset 0 -18px 28px -16px rgba(var(--gf-orange), 0.6),  /* under-glow rising from the bottom */
      inset 0 0 0 1px rgba(var(--gf-gold), 0.24);            /* warm rim */
  }
  .pill:hover {
    background:
      radial-gradient(85% 150% at 50% 128%, rgba(var(--gf-orange), 0.62), rgba(var(--gf-coral), 0.18) 46%, transparent 66%),
      linear-gradient(180deg, rgba(var(--gf-ink), 0.62) 30%, rgba(var(--gf-coral), 0.16) 100%);
  }
  /* Status dot anchored far-left so the centered "quill" wordmark stays dead-centre. Centred via
     top/bottom auto-margins (not transform) so the pulse animation's scale() doesn't fight it. */
  .pill .dot { position: absolute; left: 16px; top: 0; bottom: 0; margin: auto 0; }

  .panel {
    width: 100vw; height: 100vh; box-sizing: border-box; display: flex; flex-direction: column;
    /* Warm glass: a coral→gold wash + a soft top glow, over a heavily-frosted translucent base.
       The high blur + saturation frosts the busy desktop behind (keeping text readable) while the
       warm tint and inner glow give it an amber-through-water look. */
    /* Clear warm glass: only a WHISPER of warm tint at the very top; the body is essentially
       clear so the frosted desktop behind reads through. Legibility lives in the bubbles/cards —
       they carry their own tint — so the panel itself can stay see-through. (The remaining soft
       frost is the native macOS vibrancy applied in Rust; lighten that too if this isn't enough.) */
    /* Warm amber glass — an amber warmth + the frosted, see-through glass: a bright amber
       wash from the top over a WARM (not neutral) translucent base, a diagonal specular sheen, a
       bright top edge, and the warm glow bloom. Translucent enough that the desktop still frosts through. */
    position: relative;
    /* Dark translucent glass — the desktop shows through blurred. Orange is NOT a body fill; it
       lives ONLY in the tight top bloom (first layer) — orange = a light source at the top edge,
       not a background colour. `at 50% -18px` seats the light just above the top; `transparent 78%`
       makes it fade fast (~130px) instead of filling the panel. */
    background:
      radial-gradient(130% 150px at 50% -24px,
        rgba(255,150,60,.85) 0%, rgba(230,105,32,.5) 30%, rgba(150,68,30,.22) 55%, rgba(90,48,26,.08) 74%, transparent 88%),
      linear-gradient(to bottom, rgba(190,88,34,.10), rgba(120,58,30,.04) 28%, transparent 50%),
      rgba(24,17,13,.46);
    -webkit-backdrop-filter: blur(18px) saturate(1.35);
    backdrop-filter: blur(18px) saturate(1.35);
    border-radius: 0 0 20px 20px;
    box-shadow:
      0 24px 80px rgba(0,0,0,.6),
      inset 0 0 0 1px rgba(255,255,255,.10),
      inset 0 1px 0 rgba(255,255,255,.18);
    overflow: hidden;
  }
  .inner { flex: 1; min-height: 0; display: flex; flex-direction: column; position: relative; z-index: 1; }
  .inner.closing { opacity: 0; transition: opacity 140ms ease; }
  .toolbar {
    display: flex; align-items: center; gap: 6px; padding: 9px 13px;
    background: transparent; /* the top bloom (in .panel) shows through — no separate orange here */
    /* no divider — the header blends straight into the panel */
  }
  /* Small, flat, NEUTRAL glass icon buttons — no outline, just a barely-there frosted circle. */
  .ic {
    width: 28px; height: 28px; border-radius: 50%; display: grid; place-items: center; cursor: pointer;
    background: rgba(255,255,255,0.06); border: 0; color: #d6d7dd; padding: 0;
    -webkit-backdrop-filter: blur(12px); backdrop-filter: blur(12px);
  }
  .ic:hover { background: rgba(255,255,255,0.13); }
  .ic.active { background: rgba(255,122,46,0.20); color: #ffb98a; }
  .ic.fish { background: rgba(255,138,48,0.20); box-shadow: 0 2px 10px rgba(226,87,27,.3), inset 0 1px 0 rgba(255,255,255,.25); }
  .ic.fish.empty { background: rgba(255,255,255,0.05); } /* fish is out — show the empty dock circle */
  .ic svg { width: 15px; height: 15px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; }
  .ic.fish svg { width: 19px; height: 15px; stroke: none; }
  .spacer { flex: 1; }
  .esc { background: rgba(255,255,255,0.1); color: #d6d7dd; border: 0; border-radius: 999px; padding: 6px 14px; font-size: 12px; cursor: pointer; }
  .esc:hover { background: rgba(255,255,255,0.18); }

  .inboxbtn { position: relative; }
  .badge {
    position: absolute; top: -4px; right: -4px; min-width: 16px; height: 16px; border-radius: 999px;
    background: #ff7a2e; color: #1a1205; font-size: 10px; font-weight: 700;
    display: grid; place-items: center; padding: 0 4px; box-sizing: border-box;
  }
  .drop {
    position: absolute; top: 54px; z-index: 30; border-radius: 14px; overflow: hidden;
    /* OPAQUE — an overlay must fully cover the chat behind it, or the greeting/starters bleed through. */
    background: rgba(20,15,12,0.95);
    border: 1px solid rgba(var(--gf-gold),0.16);
    -webkit-backdrop-filter: blur(30px) saturate(1.3); backdrop-filter: blur(30px) saturate(1.3);
    box-shadow: 0 18px 50px rgba(0,0,0,0.55), inset 0 1px 0 rgba(255,255,255,0.10);
  }
  .drop.menu { left: 58px; min-width: 170px; padding: 6px; display: flex; flex-direction: column; gap: 2px; }
  .dropitem {
    text-align: left; background: none; border: 0; color: #ededf0; font: inherit; font-size: 13px;
    padding: 8px 12px; border-radius: 9px; cursor: pointer;
  }
  .dropitem:hover { background: rgba(255,255,255,0.10); }
  .dropitem.sel { background: rgba(255,122,46,0.20); }
  .drop.inbox { left: 10px; right: 10px; max-height: 340px; display: flex; flex-direction: column; }
  .inboxbar { display: flex; align-items: center; gap: 8px; padding: 9px 12px; border-bottom: 1px solid rgba(255,255,255,0.09); }
  .inboxbar .searchic { width: 15px; height: 15px; fill: none; stroke: #9a9ba3; stroke-width: 2; flex: none; }
  .inboxbar input { flex: 1; background: none; border: 0; outline: none; color: #ededf0; font: inherit; font-size: 13px; }
  .markread { background: none; border: 0; color: #9a9ba3; font-size: 12px; cursor: pointer; }
  .markread:hover { color: #ededf0; }
  .inboxlist { overflow-y: auto; padding: 4px 0 6px; }
  .inrow {
    width: 100%; display: flex; align-items: center; gap: 9px; text-align: left;
    background: none; border: 0; color: #cfd0d6; font: inherit; font-size: 13px;
    padding: 8px 14px; cursor: pointer;
  }
  .inrow:hover { background: rgba(255,255,255,0.07); }
  .inrow.unread { color: #fff; font-weight: 600; }
  .indot { width: 7px; height: 7px; border-radius: 50%; background: transparent; flex: none; }
  .indot.on { background: #ff7a2e; }
  .intitle { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .intime { color: #7e8089; font-size: 11px; font-weight: 400; flex: none; }
  .inbody { padding: 2px 30px 10px; color: #9a9ba3; font-size: 12px; white-space: pre-wrap; }

  .content { flex: 1; overflow-y: auto; padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
  /* Quiet glass-on-glass bubbles (Dan's spec): whisper fill + bright hairline edge + 1px top
     inner highlight. NO own blur/shadow — the panel already has backdrop-filter, so these just
     tint what's already frosted. Assistant = full-width & quietest; user = compact & a touch more present. */
  .bubble {
    align-self: stretch; padding: 14px 16px; border-radius: 18px; white-space: pre-wrap;
    color: rgba(255,255,255,0.9); font-size: 12.5px;
    background: rgba(255,255,255,0.04);
    border: 1px solid rgba(255,255,255,0.09);
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.08);
  }
  .bubble.user {
    align-self: flex-start; max-width: 300px; padding: 11px 15px; border-radius: 16px;
    color: rgba(255,255,255,0.92);
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.12);
    box-shadow: inset 0 1px 0 rgba(255,255,255,0.12);
  }
  .bubble.thinking { color: #9a9ba3; font-style: italic; }
  /* rendered-markdown answers (assistant bubbles use {@html mdToHtml(...)}) */
  .bubble.assistant { white-space: normal; }
  .bubble.assistant :global(p) { margin: 0 0 0.5em; }
  .bubble.assistant :global(p:last-child) { margin-bottom: 0; }
  .bubble.assistant :global(ul) { margin: 0.35em 0; padding-left: 1.15em; }
  .bubble.assistant :global(li) { margin: 0.25em 0; }
  .bubble.assistant :global(strong) { font-weight: 600; }
  .bubble.assistant :global(.mdh) { font-weight: 600; margin: 0.5em 0 0.25em; }
  .bubble.assistant :global(code) {
    background: rgba(255,255,255,0.09); padding: 0.05em 0.35em; border-radius: 4px;
    font-size: 0.88em; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .starters { display: flex; flex-direction: column; gap: 6px; align-items: flex-start; margin-top: 2px; }
  .starter {
    background: rgba(255,255,255,0.05); border: 1px solid rgba(255,255,255,0.12); color: rgba(255,255,255,0.78);
    border-radius: 999px; padding: 7px 13px; font: inherit; font-size: 12.5px; cursor: pointer; text-align: left;
  }
  .starter:hover { background: rgba(255,255,255,0.10); color: #fff; }

  .composer { display: flex; gap: 9px; padding: 12px 16px 16px; border-top: 1px solid rgba(var(--gf-gold),0.14); }
  .composer input { flex: 1; background: rgba(255,255,255,0.08); -webkit-backdrop-filter: blur(24px) saturate(1.2); backdrop-filter: blur(24px) saturate(1.2); border: 1px solid rgba(255,255,255,0.16); color: #ededf0; border-radius: 999px; padding: 10px 16px; font-size: 13px; outline: none; box-shadow: inset 0 1px 0 rgba(255,255,255,0.22), 0 8px 24px rgba(0,0,0,0.2); }
  .composer input::placeholder { color: rgba(255, 200, 160, 0.5); }
  .composer input:focus { border-color: rgba(var(--gf-orange),0.75); box-shadow: inset 0 1px 0 rgba(255,255,255,0.08), 0 0 0 3px rgba(var(--gf-orange),0.15); }
  .send { width: 34px; height: 34px; border-radius: 50%; border: 0; background: rgba(255,255,255,0.08); -webkit-backdrop-filter: blur(12px); backdrop-filter: blur(12px); color: #ededf0; cursor: pointer; display: grid; place-items: center; flex: none; box-shadow: inset 0 1px 0 rgba(255,255,255,0.2); }
  .send:hover { background: rgba(255,255,255,0.14); }
  .send svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }

  /* The manage window: persistent sidebar (left) + a body that hosts every section. */
  .bigrow { flex: 1; min-height: 0; display: flex; flex-direction: row; }
  .bigbody { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; overflow: hidden; }
  /* Constellation fills the body; other sections just use the panel width. */
  .bigbody:not(.graphfull) > :global(.content) { width: 100%; box-sizing: border-box; }
  .setcards { gap: 12px; }
  /* Compact sidebar — the manage panel stays the SAME size as chat (600px), so the nav is narrow. */
  .setnav {
    width: 128px; flex: none; display: flex; flex-direction: column; gap: 1px;
    padding: 12px 8px; border-right: 1px solid rgba(255,255,255,0.08); overflow-y: auto;
  }
  .setnav h1 { font-size: 16px; font-weight: 700; margin: 2px 0 9px 8px; }
  .setnav button {
    display: flex; align-items: center; gap: 8px;
    text-align: left; background: none; border: 0; color: #cfd0d6; font: inherit; font-size: 12.5px;
    padding: 6px 9px; border-radius: 8px; cursor: pointer; white-space: nowrap;
  }
  .setnav button svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; flex: none; }
  .setnav button:hover { background: rgba(255,255,255,0.08); }
  .setnav button.sel { background: rgba(255,122,46,0.20); color: #ff9a5b; font-weight: 600; }
  .setbody { flex: 1; min-width: 0; overflow-y: auto; display: flex; flex-direction: column; gap: 14px; padding: 22px 26px; max-width: 720px; }
  .setcard {
    background: rgba(255,255,255,0.05); border: 1px solid rgba(255,255,255,0.10);
    border-radius: 13px; padding: 12px 14px; box-shadow: inset 0 1px 0 rgba(255,255,255,0.07);
  }
  .setcard h3 { font-size: 13px; }
  .setcard h3 { margin: 0 0 4px; font-size: 13px; }
  .cardlabel { text-transform: uppercase; letter-spacing: 0.07em; font-size: 11px !important; color: #ff9a5b; }
  .permrow { display: flex; align-items: center; justify-content: space-between; gap: 10px; padding: 6px 0; }
  .permstate { display: inline-flex; align-items: center; gap: 6px; color: #9a9ba3; font-size: 12px; flex: none; }
  .permstate.ok { color: #ededf0; font-weight: 600; }
  .ghost { background: rgba(255,255,255,0.08); color: #ededf0; border: 1px solid rgba(255,255,255,0.14); border-radius: 9px; padding: 5px 11px; font-size: 12px; cursor: pointer; }
  .setcard.danger { border-color: rgba(255,107,107,0.35); }
  .dangertitle { color: #ff6b6b; }
  .dangerbtn {
    background: rgba(255,107,107,0.12); color: #ff8585; border: 1px solid rgba(255,107,107,0.4);
    border-radius: 10px; padding: 8px 14px; font-size: 13px; cursor: pointer;
  }
  .dangerbtn:hover { background: rgba(255,107,107,0.22); }
  .scrow { display: flex; align-items: center; gap: 10px; padding: 6px 0; font-size: 12.5px; color: #cfd0d6; }
  .key { background: rgba(255,255,255,0.09); border: 1px solid rgba(255,255,255,0.14); border-radius: 6px; padding: 2px 8px; font-size: 11px; white-space: nowrap; flex: none; }
  .settings section { padding: 12px 0; border-top: 1px solid rgba(255,255,255,0.09); }
  .settings section:first-child { border-top: 0; padding-top: 4px; }
  h2 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.07em; color: #9a9ba3; margin: 0 0 4px; }
  .hint { color: #7e8089; margin: 2px 0 10px; font-size: 12px; }
  em { color: #cfd0d6; font-style: normal; font-weight: 600; }
  .row { display: flex; gap: 8px; }
  .row input { flex: 1; background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.14); color: #ededf0; border-radius: 9px; padding: 8px 11px; font-size: 13px; outline: none; box-shadow: inset 0 1px 0 rgba(255,255,255,0.07); }
  .row input:focus { border-color: #ff7a2e; }
  .primary { background: #ff7a2e; color: #1a1205; border: 0; border-radius: 9px; padding: 8px 15px; font-weight: 600; cursor: pointer; font-size: 13px; }
  .toggle { display: flex; align-items: center; gap: 9px; cursor: pointer; font-weight: 600; }
  .toggle input { width: 16px; height: 16px; accent-color: #ff7a2e; }
  .chips { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 10px; }
  .chip { background: rgba(255,255,255,0.07); border: 1px solid rgba(255,255,255,0.14); border-radius: 999px; padding: 3px 6px 3px 11px; font-size: 12px; display: inline-flex; align-items: center; gap: 4px; box-shadow: inset 0 1px 0 rgba(255,255,255,0.08); }
  .x { background: none; border: 0; color: #9a9ba3; cursor: pointer; font-size: 15px; line-height: 1; }
  .x:hover { color: #ff6b6b; }
  .empty { color: #6b6d75; font-size: 12px; }
  .graphview { display: flex; flex-direction: column; gap: 8px; }
  .gchips { margin: 0; }
  .gchip { cursor: pointer; font: inherit; color: inherit; }
  .gchip.sel { background: rgba(255,122,46,0.22); border-color: rgba(255,122,46,0.55); }
  .gchip:hover { background: rgba(255,255,255,0.12); }
  .cdot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; margin-right: 5px; }
  .cnt { color: #9a9ba3; }
  .gwrap { position: relative; flex: 1; min-height: 220px; }
  .gwrap canvas { width: 100%; height: 100%; display: block; cursor: pointer; }
  .gcard {
    position: absolute; left: 8px; right: 8px; bottom: 8px;
    background: rgba(24, 19, 15, 0.55); border: 1px solid rgba(255,160,80,0.32);
    border-radius: 12px; padding: 10px 12px;
    -webkit-backdrop-filter: blur(26px) saturate(1.5); backdrop-filter: blur(26px) saturate(1.5);
    box-shadow: 0 8px 26px rgba(0,0,0,0.35), inset 0 1px 0 rgba(255,255,255,0.12);
  }
  .gcard-head { display: flex; align-items: center; gap: 7px; }
  .gcard-head .d { color: #9a9ba3; font-size: 11px; }
  .gcard p { margin: 6px 0 8px; color: #cfd0d6; font-size: 12px; }
  .gsmall { padding: 5px 11px; font-size: 12px; }
  .gfoot { text-align: right; font-size: 11px; color: #cfd0d6; }
  .gfoot .d { color: #7e8089; }
  .gcenter { text-align: center; margin-top: 90px; }
  .pfield { width: 100%; box-sizing: border-box; margin-top: 8px; resize: vertical; background: rgba(255,255,255,0.06); border: 1px solid rgba(255,255,255,0.1); color: #ededf0; border-radius: 9px; padding: 8px 11px; font: inherit; font-size: 12px; outline: none; }
  .pfield:focus { border-color: #ff7a2e; }
  .pclass { font-size: 10px; text-transform: uppercase; letter-spacing: 0.06em; color: #ff9a5b; background: rgba(255,122,46,0.14); border-radius: 999px; padding: 2px 7px; margin-left: 6px; }
  .wsum { margin: 6px 0 4px; color: #cfd0d6; font-size: 12.5px; line-height: 1.55; }
  .circlerow { display: flex; align-items: center; gap: 8px; margin-top: 8px; font-size: 12px; color: #9a9ba3; }
  .circinput { flex: 1; background: rgba(255,255,255,0.06); border: 1px solid rgba(255,255,255,0.1); color: #ededf0; border-radius: 9px; padding: 6px 10px; font: inherit; font-size: 12px; outline: none; }
  .circinput:focus { border-color: #ff7a2e; }
  /* Profiles: voice list, provenance examples, preview, consent, review-diff */
  .voicelist { margin: 4px 0 4px; padding-left: 18px; color: #d3d4da; font-size: 12.5px; line-height: 1.5; }
  .voicelist li { margin: 2px 0; }
  .examples { margin: 4px 0 6px; padding-left: 16px; color: #9a9ba3; font-size: 12px; line-height: 1.5; }
  .linkbtn { background: none; border: none; color: #ff9a5b; cursor: pointer; font: inherit; font-size: 12px; padding: 0; text-decoration: underline; }
  .prow { justify-content: space-between; align-items: center; margin-top: 8px; }
  .toggle.small { font-weight: 400; font-size: 12px; color: #cfd0d6; }
  .preview { margin-top: 8px; background: rgba(255,122,46,0.08); border: 1px solid rgba(255,122,46,0.28); border-radius: 10px; padding: 9px 12px; }
  .preview p { margin: 5px 0 0; color: #ededf0; font-size: 12.5px; line-height: 1.55; white-space: pre-wrap; }
  .difflabel { font-size: 10px; text-transform: uppercase; letter-spacing: 0.06em; color: #9a9ba3; }
  .difflabel.dnew { color: #ff9a5b; }
  .review { border-color: rgba(255,122,46,0.35); background: rgba(255,122,46,0.06); }
  .diff { display: flex; gap: 10px; margin: 8px 0; }
  .diffcol { flex: 1; min-width: 0; }
  .diff pre { margin: 4px 0 0; white-space: pre-wrap; word-break: break-word; background: rgba(255,255,255,0.05); border: 1px solid rgba(255,255,255,0.09); border-radius: 8px; padding: 8px 10px; color: #d3d4da; font-size: 11.5px; line-height: 1.5; font-family: inherit; }
  .pfield.tall { min-height: 220px; line-height: 1.55; }
  .blurb { margin-top: 10px; }
  .blurb summary { color: #9a9ba3; font-size: 11.5px; cursor: pointer; }
  .blurb pre { margin: 6px 0 0; white-space: pre-wrap; background: rgba(255,255,255,0.05); border: 1px solid rgba(255,255,255,0.09); border-radius: 8px; padding: 8px 10px; color: #cfd0d6; font-size: 11.5px; line-height: 1.5; font-family: inherit; }
  .statusline { display: flex; align-items: center; gap: 7px; margin-bottom: 4px; font-weight: 600; }
  .statusline .d { color: #9a9ba3; font-weight: 400; }
  .fact { background: rgba(255,255,255,0.06); border: 1px solid rgba(255,255,255,0.09); border-radius: 10px; padding: 9px 12px; margin-top: 8px; box-shadow: inset 0 1px 0 rgba(255,255,255,0.06); }
  .fact p { margin: 6px 0 0; color: #cfd0d6; font-size: 12px; white-space: pre-wrap; }
  .chip.src { background: rgba(255,122,46,0.16); border-color: rgba(255,122,46,0.4); }
  .segs { display: flex; flex-direction: column; gap: 4px; }
  .seg { display: flex; justify-content: space-between; background: rgba(255,255,255,0.06); border: 1px solid rgba(255,255,255,0.09); border-radius: 8px; padding: 7px 11px; box-shadow: inset 0 1px 0 rgba(255,255,255,0.06); }
  .seg .d { color: #9a9ba3; }
</style>
