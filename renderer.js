const tokenValue = document.getElementById("tokenValue");
const smallStat = document.getElementById("smallStat");
const refreshButton = document.getElementById("refreshButton");
const pinButton = document.getElementById("pinButton");
const minimizeButton = document.getElementById("minimizeButton");
const layoutButton = document.getElementById("layoutButton");
const mascotLayer = document.getElementById("mascotLayer");
const petCard = document.querySelector(".pet-card");
const tabButtons = document.querySelectorAll(".tab-btn");

const invoke = window.__TAURI__.tauri.invoke;
const appWindow = window.__TAURI__.window?.appWindow;

let isPinned = true;
let isCompact = localStorage.getItem("tokenPetLayout") === "compact";
let isEdgeDocked = false;
let edgeDockSide = null;
let collapseTimer = 0;
let moveSettleTimer = 0;
let suppressMoveSettleUntil = 0;
let dragFinalizeTimer = 0;
let manualDrag = null;
let dragMoveFrame = 0;
let currentPeriod = "today";
pinButton.classList.add("is-active");

function formatCost(value) {
  return `$${Number(value || 0).toFixed(4)}`;
}

function setPinState(nextPinned) {
  isPinned = nextPinned;
  pinButton.classList.toggle("is-active", isPinned);
  pinButton.title = isPinned ? "\u7f6e\u9876\u4e2d" : "\u672a\u7f6e\u9876";
  pinButton.setAttribute("aria-label", pinButton.title);
}

function setCompactState(nextCompact) {
  isCompact = nextCompact;
  petCard.classList.toggle("is-compact", isCompact);
  layoutButton.classList.toggle("is-active", isCompact);
  layoutButton.setAttribute("aria-pressed", String(isCompact));
  layoutButton.title = isCompact ? "\u5207\u6362\u539f\u7248" : "\u5207\u6362\u7d27\u51d1\u7248";
  layoutButton.setAttribute("aria-label", layoutButton.title);
  localStorage.setItem("tokenPetLayout", isCompact ? "compact" : "default");
}

function setEdgeDockState(docked, edge = null) {
  isEdgeDocked = docked;
  edgeDockSide = docked ? edge : null;
  petCard.classList.toggle("is-edge-docked", isEdgeDocked);
  petCard.classList.remove("is-hover-expanded");
  if (edgeDockSide) {
    petCard.dataset.edge = edgeDockSide;
  } else {
    delete petCard.dataset.edge;
  }
}

window.applyEdgeDockFromHost = (edge) => {
  setEdgeDockState(true, edge || "right");
};

window.applyEdgeUndockFromHost = () => {
  setEdgeDockState(false);
};

async function applyWindowLayout(nextCompact) {
  setCompactState(nextCompact);
  if (!isEdgeDocked) {
    suppressMoveSettle();
    await invoke("set_compact_mode", { compact: isCompact }).catch(() => {});
  }
}

function loadMascot() {
  const image = new Image();
  image.onload = () => {
    mascotLayer.src = `./assets/mascot-cutout.png?v=${Date.now()}`;
  };
  image.src = `./assets/mascot-cutout.png?v=${Date.now()}`;
}

async function refreshStats() {
  try {
    const stats = await invoke("get_stats", { period: currentPeriod });
    const oldValue = tokenValue.textContent;
    const newValue = stats.totalTokensText;

    // Spin refresh icon
    refreshButton.classList.add("is-spinning");
    window.setTimeout(() => refreshButton.classList.remove("is-spinning"), 300);

    // Update values
    tokenValue.textContent = newValue;
    smallStat.innerHTML = `${stats.requestCount} \u6b21\u8bf7\u6c42&nbsp;&nbsp;${formatCost(stats.totalCostUsd)}<br>${Math.round(stats.successRate)}% \u6210\u529f`;

    // Animate token value if changed
    if (oldValue !== newValue) {
      tokenValue.classList.add("animating");
      window.setTimeout(() => tokenValue.classList.remove("animating"), 300);
    }
  } catch (error) {
    tokenValue.textContent = "0";
    smallStat.textContent = "\u8bfb\u53d6\u5931\u8d25";
  }
}

window.refreshStats = refreshStats;

function finalizeEdgeDrag() {
  window.clearTimeout(dragFinalizeTimer);
  manualDrag = null;
  invoke("finish_edge_drag")
    .then((state) => setEdgeDockState(Boolean(state.docked), state.edge || null))
    .catch(() => {});
}

async function startWindowDrag(event) {
  if (event.button !== 0 && event.pointerType !== "touch") return;
  if (event.target.closest(".tools, button, .tab-btn")) return;

  event.preventDefault();
  window.clearTimeout(collapseTimer);
  event.target.setPointerCapture?.(event.pointerId);

  manualDrag = {
    pointerId: event.pointerId,
    offsetX: event.clientX,
    offsetY: event.clientY,
    screenX: event.screenX,
    screenY: event.screenY,
  };

  if (isEdgeDocked && edgeDockSide) {
    suppressMoveSettle();
    invoke("begin_edge_drag", {
      cursorOffsetX: event.clientX,
      cursorOffsetY: event.clientY,
      screenX: event.screenX,
      compact: isCompact,
      dockedEdge: edgeDockSide,
    }).catch(() => {});
  } else {
    invoke("begin_edge_drag", {
      cursorOffsetX: event.clientX,
      cursorOffsetY: event.clientY,
      screenX: event.screenX,
      compact: isCompact,
      dockedEdge: null,
    }).catch(() => {});
  }
}

function moveManualDrag(event) {
  if (!manualDrag || event.pointerId !== manualDrag.pointerId) return;
  manualDrag.screenX = event.screenX;
  manualDrag.screenY = event.screenY;
  if (dragMoveFrame) return;
  dragMoveFrame = window.requestAnimationFrame(() => {
    dragMoveFrame = 0;
    if (!manualDrag) return;
    invoke("drag_move_window", {
      screenX: manualDrag.screenX,
      screenY: manualDrag.screenY,
      cursorOffsetX: manualDrag.offsetX,
      cursorOffsetY: manualDrag.offsetY,
    }).catch(() => {});
  });
}

document.addEventListener("pointerdown", startWindowDrag, true);
document.addEventListener("pointermove", moveManualDrag, true);
document.addEventListener("pointerup", finalizeEdgeDrag, true);
document.addEventListener("pointercancel", finalizeEdgeDrag, true);
window.addEventListener("blur", () => {
  dragFinalizeTimer = window.setTimeout(finalizeEdgeDrag, 120);
});

function suppressMoveSettle(duration = 260) {
  suppressMoveSettleUntil = Date.now() + duration;
}

function scheduleEdgeDockSettle(delay = 180) {
  window.clearTimeout(moveSettleTimer);
  moveSettleTimer = window.setTimeout(() => {
    if (Date.now() < suppressMoveSettleUntil) return;
    settleEdgeDock();
  }, delay);
}

async function settleEdgeDock() {
  const state = await invoke("settle_edge_dock").catch(() => ({ docked: false, edge: null }));
  setEdgeDockState(Boolean(state.docked), state.edge || null);
}

refreshButton.addEventListener("click", refreshStats);
tabButtons.forEach(btn => {
  btn.addEventListener("click", () => {
    tabButtons.forEach(b => b.classList.remove("active"));
    btn.classList.add("active");
    currentPeriod = btn.dataset.period;
    refreshStats();
  });
});
pinButton.addEventListener("click", async () => {
  const nextPinned = await invoke("toggle_top");
  setPinState(Boolean(nextPinned));
});
minimizeButton.addEventListener("click", () => invoke("hide_window"));
layoutButton.addEventListener("click", () => applyWindowLayout(!isCompact));

applyWindowLayout(isCompact);
setPinState(true);
loadMascot();
refreshStats();
setInterval(refreshStats, 30000);

// --- Right-click: show popup menu window at cursor ---
document.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  invoke("show_context_menu", { x: e.screenX, y: e.screenY });
});
