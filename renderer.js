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

async function applyWindowLayout(nextCompact) {
  setCompactState(nextCompact);
  await invoke("set_compact_mode", { compact: isCompact }).catch(() => {});
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
    tokenValue.textContent = stats.totalTokensText;
    smallStat.innerHTML = `${stats.requestCount} \u6b21\u8bf7\u6c42&nbsp;&nbsp;${formatCost(stats.totalCostUsd)}<br>${Math.round(stats.successRate)}% \u6210\u529f`;
    refreshButton.classList.add("is-pulsing");
    window.setTimeout(() => refreshButton.classList.remove("is-pulsing"), 220);
  } catch (error) {
    tokenValue.textContent = "0";
    smallStat.textContent = "\u8bfb\u53d6\u5931\u8d25";
  }
}

window.refreshStats = refreshStats;

async function startWindowDrag(event) {
  if (event.button !== 0) return;
  if (event.target.closest(".tools, button, .tab-btn")) return;

  event.preventDefault();

  try {
    if (appWindow?.startDragging) {
      await appWindow.startDragging();
      return;
    }
  } catch (_) {
    // Fall back to the Rust command below.
  }

  await invoke("start_dragging").catch(() => {});
}

document.addEventListener("mousedown", startWindowDrag, true);

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
