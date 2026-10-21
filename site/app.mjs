import { daylightAt } from "./daylight.mjs";

const clock = new Intl.DateTimeFormat("en-GB", {
  timeZone: "Arctic/Longyearbyen",
  hour: "2-digit",
  minute: "2-digit",
  hourCycle: "h23",
});
const themeInputs = document.querySelectorAll('input[name="theme"]');
let appearance = "auto";
try {
  const saved = localStorage.getItem("svalbard-appearance");
  if (["auto", "light", "dark"].includes(saved)) appearance = saved;
} catch {
  /* Storage can be disabled in a private browser. Auto still works. */
}

function updateDaylight() {
  const now = new Date();
  const sun = daylightAt(now);
  document.documentElement.dataset.theme =
    appearance === "auto" ? sun.theme : appearance;
  document.querySelector("#arctic-clock").textContent =
    `${clock.format(now)} in Longyearbyen`;
  document.querySelector("#arctic-phase").textContent =
    `${sun.phase} · ${appearance === "auto" ? "Auto" : `${appearance === "light" ? "Day" : "Night"} preview`}`;
}

document.querySelector(".theme-control").disabled = false;
for (const input of themeInputs) {
  input.checked = input.value === appearance;
  input.addEventListener("change", () => {
    appearance = input.value;
    try {
      localStorage.setItem("svalbard-appearance", appearance);
    } catch {
      /* Optional preference. */
    }
    updateDaylight();
  });
}
updateDaylight();
setInterval(updateDaylight, 60000);

const database = document.querySelector("#database");
const destination = document.querySelector("#destination");
const output = document.querySelector("#terminal-lines");
const playButton = document.querySelector("#play-demo");
const playLabel = document.querySelector("#play-label");
const playIcon = document.querySelector("#play-icon");
const announcement = document.querySelector("#demo-announcement");
const progressFill = document.querySelector("#progress-fill");
const progressNumber = document.querySelector("#progress-number");
const caption = document.querySelector("#terminal-caption");
const journeySteps = [...document.querySelectorAll(".journey-step")];
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
const duration = 13000;
let elapsed = 0;
let playing = false;
let frame = 0;
let previousTick = null;
let shownStage = -1;
let resumeWhenVisible = false;

function stages() {
  const source = database.options[database.selectedIndex].text;
  const target = destination.options[destination.selectedIndex].text;
  return [
    {
      at: 0,
      text: `Connecting to ${source}`,
      caption: "Starting a bounded snapshot.",
      step: 0,
    },
    {
      at: 1600,
      text: "Snapshot started · one worker",
      caption: "Writing to private local staging.",
      step: 0,
    },
    {
      at: 4400,
      text: "Dump staged · snapshot closed",
      caption: "Database work is finished.",
      step: 1,
    },
    {
      at: 6300,
      text: `Uploading to ${target}`,
      caption: `Transferring the staged file to ${target}.`,
      step: 2,
    },
    {
      at: 10700,
      text: "Upload complete · checking result",
      caption: "Confirming the stored backup.",
      step: 2,
    },
    {
      at: duration,
      text: "Backup stored · local staging cleared",
      caption: "Example complete.",
      step: 3,
    },
  ];
}

function updateTransport() {
  const complete = elapsed >= duration;
  playLabel.textContent = playing
    ? "Pause"
    : complete
      ? "Replay"
      : elapsed > 0
        ? "Resume"
        : "Run example";
  playIcon.textContent = playing ? "Ⅱ" : complete ? "↻" : "▶";
}

function render() {
  const progress = Math.min(100, Math.round((elapsed / duration) * 100));
  progressFill.style.width = `${progress}%`;
  progressNumber.textContent = `${progress}%`;
  const sequence = stages();
  const currentStage = sequence.findLastIndex((stage) => elapsed >= stage.at);
  if (currentStage === shownStage) return;
  shownStage = currentStage;
  output.replaceChildren();
  sequence.slice(0, currentStage + 1).forEach((stage, index) => {
    const line = document.createElement("p");
    line.className = `terminal-line${index === currentStage ? " enter" : ""}`;
    for (const [className, text] of [
      [
        "log-time",
        `00:${String(Math.floor(stage.at / 1000)).padStart(2, "0")}`,
      ],
      [
        "log-symbol",
        index < currentStage || currentStage === sequence.length - 1
          ? "✓"
          : "›",
      ],
      ["", stage.text],
    ]) {
      const span = document.createElement("span");
      span.className = className;
      span.textContent = text;
      line.append(span);
    }
    output.append(line);
  });
  const stage = sequence[currentStage];
  caption.textContent = stage.caption;
  announcement.textContent = `Preview: ${stage.text}. ${stage.caption}`;
  journeySteps.forEach((node, index) => {
    node.classList.toggle("active", index === stage.step);
    node.classList.toggle("complete", index < stage.step);
  });
}

function tick(now) {
  if (!playing) return;
  if (previousTick !== null)
    elapsed = Math.min(duration, elapsed + now - previousTick);
  previousTick = now;
  render();
  if (elapsed >= duration) setPlaying(false);
  else frame = requestAnimationFrame(tick);
}

function setPlaying(value) {
  playing = value;
  previousTick = null;
  cancelAnimationFrame(frame);
  if (playing) frame = requestAnimationFrame(tick);
  updateTransport();
}

function reset() {
  setPlaying(false);
  resumeWhenVisible = false;
  elapsed = 0;
  shownStage = -1;
  output.replaceChildren();
  const line = document.createElement("p");
  line.className = "terminal-line";
  line.textContent = "Waiting to start.";
  output.append(line);
  progressFill.style.width = "0%";
  progressNumber.textContent = "0%";
  caption.textContent = "Select a database and destination.";
  journeySteps.forEach((step) => step.classList.remove("active", "complete"));
  announcement.textContent =
    "Preview ready. Run it with the selected database and destination.";
  updateTransport();
}

function finishWithoutAnimation() {
  setPlaying(false);
  elapsed = duration;
  render();
  updateTransport();
}

playButton.disabled = false;
playButton.addEventListener("click", () => {
  if (reducedMotion.matches) {
    finishWithoutAnimation();
    return;
  }
  if (elapsed >= duration) reset();
  setPlaying(!playing);
});
database.addEventListener("change", reset);
destination.addEventListener("change", reset);
document.addEventListener("visibilitychange", () => {
  updateDaylight();
  if (document.hidden) {
    resumeWhenVisible = playing;
    setPlaying(false);
  } else if (resumeWhenVisible) {
    resumeWhenVisible = false;
    setPlaying(true);
  }
});
reducedMotion.addEventListener("change", () => {
  if (reducedMotion.matches) {
    resumeWhenVisible = false;
    finishWithoutAnimation();
  }
});
if (reducedMotion.matches) finishWithoutAnimation();
else if (!document.hidden) setPlaying(true);
else resumeWhenVisible = true;
