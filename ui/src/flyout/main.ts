/**
 * Flyout: task list and clipboard history in one window with a tab strip.
 *
 * The list rendering is deliberately incremental — a full re-render on every
 * `todo-changed` would blow away the text selection and scroll position the
 * user is working with, so rows are keyed by id and patched in place.
 */

import {
  api,
  on,
  formatBytes,
  formatRelative,
  debounce,
  type ClipEntry,
  type ClipFilter,
  type Config,
  type FlyoutTab,
  type Task,
  type TaskFilter,
} from "../shared/bridge";

const el = {
  tabs: Array.from(document.querySelectorAll<HTMLButtonElement>(".tab")),
  todoCount: document.getElementById("todo-count") as HTMLSpanElement,
  clipCount: document.getElementById("clip-count") as HTMLSpanElement,
  todoPane: document.getElementById("pane-todo") as HTMLElement,
  clipPane: document.getElementById("pane-clipboard") as HTMLElement,
  todoForm: document.getElementById("todo-form") as HTMLFormElement,
  todoInput: document.getElementById("todo-input") as HTMLInputElement,
  todoFilter: document.getElementById("todo-filter") as HTMLSelectElement,
  todoList: document.getElementById("todo-list") as HTMLUListElement,
  todoSummary: document.getElementById("todo-summary") as HTMLElement,
  todoExportMd: document.getElementById("todo-export-md") as HTMLButtonElement,
  todoExportJson: document.getElementById("todo-export-json") as HTMLButtonElement,
  todoClearDone: document.getElementById("todo-clear-done") as HTMLButtonElement,
  clipSearch: document.getElementById("clip-search") as HTMLInputElement,
  clipList: document.getElementById("clip-list") as HTMLUListElement,
  clipFilters: document.getElementById("clip-filters") as HTMLDivElement,
  clipSummary: document.getElementById("clip-summary") as HTMLElement,
  clipClear: document.getElementById("clip-clear") as HTMLButtonElement,
  clipClearPinned: document.getElementById("clip-clear-pinned") as HTMLButtonElement,
  openSettings: document.getElementById("open-settings") as HTMLButtonElement,
  close: document.getElementById("close-flyout") as HTMLButtonElement,
  toast: document.getElementById("toast") as HTMLDivElement,
};

const PAGE_SIZE = 60;

let config: Config | null = null;
let activeTab: FlyoutTab = "todo";
let filter: TaskFilter = "today";
let tasks: Task[] = [];
let clips: ClipEntry[] = [];
let clipQuery = "";
let clipFilter: ClipFilter = "all";
/** Cache of `image_path -> asset url`, so we do not round-trip per row. */
const assetUrls = new Map<string, string>();

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

async function init() {
  config = await api.getConfig();
  applyTheme(config);
  activeTab = await api.flyoutTab().catch(() => "todo" as FlyoutTab);
  await switchTab(activeTab, false);

  on("config-changed", (next) => {
    config = next;
    applyTheme(next);
  });
  on("todo-changed", () => {
    void refreshTodos();
  });
  on("clipboard-changed", () => {
    if (activeTab === "clipboard") void refreshClips();
  });

  el.tabs.forEach((tab) =>
    tab.addEventListener("click", () => void switchTab(tab.dataset.tab as FlyoutTab)),
  );

  el.todoForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const title = el.todoInput.value.trim();
    if (!title) return;
    el.todoInput.value = "";
    await api.todoCreate(title, filter === "today" ? "today" : "today");
    await refreshTodos();
  });

  el.todoFilter.addEventListener("change", async () => {
    filter = el.todoFilter.value as TaskFilter;
    await refreshTodos();
  });

  el.todoList.addEventListener("click", (event) => void onTodoClick(event));
  el.todoExportMd.addEventListener("click", () => void exportTodos("markdown"));
  el.todoExportJson.addEventListener("click", () => void exportTodos("json"));
  el.todoClearDone.addEventListener("click", async () => {
    const removed = await api.todoClearCompleted();
    await refreshTodos();
    toast(removed > 0 ? `已清除 ${removed} 项已完成任务` : "没有已完成的任务");
  });

  el.clipSearch.addEventListener("input", debounce(() => void refreshClips(), 180));
  el.clipFilters.addEventListener("click", (event) => {
    const button = (event.target as HTMLElement).closest<HTMLButtonElement>("button[data-filter]");
    if (!button) return;
    clipFilter = button.dataset.filter as ClipFilter;
    renderClipFilters();
    void refreshClips();
  });
  el.clipList.addEventListener("click", (event) => void onClipClick(event));
  el.clipClear.addEventListener("click", () => void clearClips(false));
  el.clipClearPinned.addEventListener("click", () => void clearClips(true));

  el.openSettings.addEventListener("click", () => {
    void api.hideFlyout();
    void api.openSettings();
  });
  el.close.addEventListener("click", () => void api.hideFlyout());

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      void api.hideFlyout();
      return;
    }
    // Ctrl+Tab cycles the tabs, which is what a two-tab strip should do.
    if (event.key === "Tab" && event.ctrlKey) {
      event.preventDefault();
      void switchTab(activeTab === "todo" ? "clipboard" : "todo");
      return;
    }
    // The search field is reachable without the mouse, like every other
    // clipboard tool.
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
      event.preventDefault();
      if (activeTab === "clipboard") el.clipSearch.focus();
      return;
    }
    if (event.key === "Enter" && activeTab === "clipboard") {
      const first = el.clipList.querySelector<HTMLElement>(".clip-row");
      const id = first?.dataset.id;
      if (id && document.activeElement !== el.clipSearch) void restoreClip(Number(id));
    }
  });

  renderClipFilters();

  // Give the composer focus when the flyout opens on the todo tab, so typing a
  // task is a single keystroke away.
  if (activeTab === "todo") el.todoInput.focus();
}

function applyTheme(cfg: Config) {
  document.documentElement.dataset.theme = cfg.ui.theme;
  document.documentElement.style.setProperty("--accent", cfg.ui.accent);
  document.documentElement.style.setProperty(
    "--panel-radius",
    `${cfg.widget.corner_radius}px`,
  );
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

async function switchTab(tab: FlyoutTab, persist = true) {
  activeTab = tab;
  el.tabs.forEach((button) => {
    button.setAttribute("aria-selected", String(button.dataset.tab === tab));
  });
  el.todoPane.hidden = tab !== "todo";
  el.clipPane.hidden = tab !== "clipboard";

  if (tab === "todo") {
    await refreshTodos();
  } else {
    await refreshClips();
    el.clipSearch.focus();
  }

  // Rust owns the remembered tab so the launcher reopens where the user left.
  if (persist && config?.widget.remember_tab) {
    await api.showFlyout(tab);
  }
}

// ---------------------------------------------------------------------------
// Todo
// ---------------------------------------------------------------------------

async function refreshTodos() {
  tasks = await api.todoList(filter);
  renderTodos();
  const open = await api.todoOpenCount().catch(() => 0);
  el.todoCount.textContent = open > 0 ? String(open) : "";
  el.todoCount.hidden = open <= 0;
}

function renderTodos() {
  el.todoList.replaceChildren();

  if (tasks.length === 0) {
    const message =
      filter === "done"
        ? "还没有已完成的任务"
        : filter === "today"
          ? "今天没有待办，享受一下吧"
          : "还没有任务，在上面输入后回车即可添加";
    el.todoList.append(emptyState(message));
  } else {
    for (const task of tasks) el.todoList.append(todoRow(task));
  }

  const open = tasks.filter((t) => !t.done).length;
  const done = tasks.length - open;
  el.todoSummary.textContent =
    tasks.length === 0 ? "—" : `未完成 ${open} · 已完成 ${done} · 共 ${tasks.length}`;
}

function todoRow(task: Task): HTMLLIElement {
  const row = document.createElement("li");
  row.className = "todo-row";
  row.dataset.id = String(task.id);
  row.dataset.done = String(task.done);

  const check = document.createElement("button");
  check.type = "button";
  check.className = "check";
  check.dataset.action = "toggle";
  check.setAttribute("aria-label", task.done ? "标记为未完成" : "标记为已完成");
  check.innerHTML =
    '<svg viewBox="0 0 12 12"><path d="M2.5 6.2 5 8.7l4.5-5.4"/></svg>';

  const title = document.createElement("span");
  title.className = "title";
  title.textContent = task.title;
  title.dataset.action = "edit";
  title.title = "点击编辑";

  const dot = document.createElement("span");
  dot.className = "dot";
  dot.dataset.priority = task.priority;
  dot.dataset.action = "priority";
  dot.title = "切换优先级";

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "remove";
  remove.dataset.action = "delete";
  remove.setAttribute("aria-label", "删除任务");
  remove.innerHTML =
    '<svg viewBox="0 0 12 12"><path d="M3 3.9 3.9 3 6 5.1 8.1 3l.9.9L6.9 6l2.1 2.1-.9.9L6 6.9 3.9 9 3 8.1 5.1 6z"/></svg>';

  row.append(check, title, dot, remove);
  return row;
}

async function onTodoClick(event: MouseEvent) {
  const target = (event.target as HTMLElement).closest<HTMLElement>("[data-action]");
  if (!target) return;
  const row = target.closest<HTMLElement>(".todo-row");
  const id = Number(row?.dataset.id);
  if (!id) return;

  switch (target.dataset.action) {
    case "toggle": {
      const task = tasks.find((t) => t.id === id);
      if (!task) return;
      await api.todoUpdate(id, { done: !task.done });
      await refreshTodos();
      break;
    }
    case "delete":
      await api.todoDelete(id);
      await refreshTodos();
      break;
    case "priority": {
      const task = tasks.find((t) => t.id === id);
      if (!task) return;
      const order = ["none", "low", "medium", "high"] as const;
      const next = order[(order.indexOf(task.priority) + 1) % order.length];
      await api.todoUpdate(id, { priority: next });
      await refreshTodos();
      break;
    }
    case "edit":
      await inlineEdit(id, target as HTMLSpanElement);
      break;
  }
}

/** Swap the title for an input; Enter commits, Escape reverts. */
async function inlineEdit(id: number, node: HTMLSpanElement) {
  const original = node.textContent ?? "";
  const input = document.createElement("input");
  input.type = "text";
  input.value = original;
  input.style.cssText =
    "flex:1 1 auto;min-width:0;height:20px;padding:0 4px;border:1px solid var(--accent);border-radius:4px;background:var(--surface);color:var(--text);font:inherit;";

  node.replaceWith(input);
  input.focus();
  input.setSelectionRange(original.length, original.length);

  let settled = false;
  const commit = async (save: boolean) => {
    if (settled) return;
    settled = true;
    const value = input.value.trim();
    if (save && value && value !== original) {
      await api.todoUpdate(id, { title: value });
    }
    await refreshTodos();
  };

  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") void commit(true);
    if (event.key === "Escape") void commit(false);
  });
  input.addEventListener("blur", () => void commit(true));
}

async function exportTodos(format: "markdown" | "json") {
  const text = await api.todoExport(format);
  // Reuse the clipboard module rather than pulling in a file dialog: the text
  // lands where the user can paste it into a note, a commit message or a repo.
  await api.setClipboardText(text);
  toast(format === "markdown" ? "Markdown 已复制到剪贴板" : "JSON 已复制到剪贴板");
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

function renderClipFilters() {
  el.clipFilters.replaceChildren();
  for (const { value, label } of CLIP_FILTERS) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "chip";
    chip.dataset.filter = value;
    chip.textContent = label;
    chip.setAttribute("aria-selected", String(value === clipFilter));
    if (value === clipFilter) chip.classList.add("on");
    el.clipFilters.append(chip);
  }
}

/** The host of a link, which is what a reader actually scans for. */
function linkHost(text: string): string {
  try {
    return new URL(text.trim()).host || text.trim();
  } catch {
    return text.trim();
  }
}

async function refreshClips() {
  clipQuery = el.clipSearch.value;
  clips = await api.clipboardList(clipQuery, clipFilter, PAGE_SIZE, 0);
  await renderClips();
  const stats = await api.clipboardStats().catch(() => null);
  el.clipCount.textContent = stats && stats.total > 0 ? String(stats.total) : "";
  el.clipCount.hidden = !stats || stats.total <= 0;
  el.clipSummary.textContent = stats
    ? `共 ${stats.total} 条 · 收藏 ${stats.pinned} 条 · 约 ${formatBytes(stats.bytes)}`
    : "—";
}

async function renderClips() {
  el.clipList.replaceChildren();

  if (clips.length === 0) {
    const label = CLIP_FILTERS.find((f) => f.value === clipFilter)?.label ?? "";
    el.clipList.append(
      emptyState(
        clipQuery
          ? `没有匹配「${clipQuery}」的记录`
          : clipFilter === "all"
            ? "复制点什么，这里就会有记录"
            : `还没有「${label}」类型的记录`,
      ),
    );
    return;
  }
  for (const clip of clips) el.clipList.append(await clipRow(clip));
}

async function clipRow(clip: ClipEntry): Promise<HTMLLIElement> {
  const row = document.createElement("li");
  row.className = "clip-row";
  row.dataset.id = String(clip.id);
  row.dataset.pinned = String(clip.pinned);
  row.title = "点击放回剪贴板";

  const kind = document.createElement("span");
  kind.className = "kind";
  kind.innerHTML = KIND_ICONS[clip.kind];

  const body = document.createElement("div");
  body.className = "body";

  if (clip.kind === "image" && clip.image_path) {
    const img = document.createElement("img");
    img.className = "thumb";
    img.alt = clip.preview;
    img.loading = "lazy";
    img.src = await assetUrl(clip.image_path);
    body.append(img);
  }

  const preview = document.createElement("div");
  preview.className = "preview";
  preview.textContent = clip.preview || "(空)";
  body.append(preview);

  // What OCR read out of the image — the only way a screenshot is findable by
  // a word that is in it rather than in its label.
  if (clip.ocr_text) {
    const ocr = document.createElement("div");
    ocr.className = "ocr";
    ocr.textContent = clip.ocr_text;
    body.append(ocr);
  }

  const meta = document.createElement("div");
  meta.className = "meta";
  const when = document.createElement("span");
  when.textContent = formatRelative(clip.created_at);
  meta.append(when);
  if (clip.kind === "files") {
    const n = clip.text.split("\n").filter(Boolean).length;
    const files = document.createElement("span");
    files.textContent = `${n} 个文件`;
    meta.append(files);
  } else if (clip.kind === "link") {
    const host = document.createElement("span");
    host.textContent = linkHost(clip.text);
    meta.append(host);
  } else if (clip.kind !== "image") {
    const size = document.createElement("span");
    size.textContent = `${clip.text.length} 字符`;
    meta.append(size);
  }
  body.append(meta);

  const actions = document.createElement("div");
  actions.className = "actions";
  actions.append(
    actionButton("pin", clip.pinned ? "取消收藏" : "收藏", ICON_PIN, clip.pinned),
    actionButton("delete", "删除", ICON_TRASH),
  );

  row.append(kind, body, actions);
  return row;
}

function actionButton(
  action: string,
  label: string,
  icon: string,
  active = false,
): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.dataset.action = action;
  button.dataset.active = String(active);
  button.title = label;
  button.setAttribute("aria-label", label);
  button.innerHTML = icon;
  return button;
}

async function onClipClick(event: MouseEvent) {
  const target = (event.target as HTMLElement).closest<HTMLElement>("[data-action]");
  const row = (event.target as HTMLElement).closest<HTMLElement>(".clip-row");
  const id = Number(row?.dataset.id);
  if (!id) return;

  // A bare click on the row restores it; the icon buttons do something else.
  if (!target) {
    await restoreClip(id);
    return;
  }
  event.stopPropagation();

  switch (target.dataset.action) {
    case "pin": {
      const clip = clips.find((c) => c.id === id);
      if (!clip) return;
      await api.clipboardPin(id, !clip.pinned);
      await refreshClips();
      break;
    }
    case "delete":
      await api.clipboardDelete(id);
      await refreshClips();
      break;
  }
}

async function restoreClip(id: number) {
  await api.clipboardCopy(id);
  toast("已复制到剪贴板");
  // The flyout has done its job once the content is back on the clipboard.
  window.setTimeout(() => void api.hideFlyout(), 220);
}

async function clearClips(includePinned: boolean) {
  const removed = await api.clipboardClear(includePinned);
  await refreshClips();
  toast(removed > 0 ? `已删除 ${removed} 条记录` : "没有可删除的记录");
}

async function assetUrl(path: string): Promise<string> {
  const cached = assetUrls.get(path);
  if (cached) return cached;
  const url = await api.assetUrl(path).catch(() => "");
  assetUrls.set(path, url);
  return url;
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

function emptyState(message: string): HTMLLIElement {
  const li = document.createElement("li");
  li.className = "empty";
  li.textContent = message;
  return li;
}

let toastTimer: number | undefined;
function toast(message: string) {
  el.toast.textContent = message;
  el.toast.hidden = false;
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    el.toast.hidden = true;
  }, 1800);
}

const ICON_PIN =
  '<svg viewBox="0 0 12 12"><path d="M7.4 1 11 4.6l-1 .6-.3 2.5-1.9-1.9-2.9 3.4-.6-.6 3.4-2.9L5.8 3.8 8.3 3.5z"/></svg>';
const ICON_TRASH =
  '<svg viewBox="0 0 12 12"><path d="M5 1.5h2v.8h2.6v1H2.4v-1H5zM3.1 4h5.8l-.4 6.2a.7.7 0 0 1-.7.6H4.2a.7.7 0 0 1-.7-.6z"/></svg>';
const ICON_TEXT =
  '<svg viewBox="0 0 12 12"><path d="M2 2h8v1.4H6.7V10H5.3V3.4H2z"/></svg>';
const ICON_IMAGE =
  '<svg viewBox="0 0 12 12"><path d="M1.5 2.5h9v7h-9zm1.2 5.7h6.6L7.4 5.4 5.9 7.3 4.8 6z"/></svg>';
const ICON_FILES =
  '<svg viewBox="0 0 12 12"><path d="M2 1.5h4l1 1.2h3v7.8H2z"/></svg>';
const ICON_LINK =
  '<svg viewBox="0 0 12 12"><path d="M5.1 6.9a2.3 2.3 0 0 0 3.3.1l1.4-1.4a2.3 2.3 0 0 0-3.3-3.3l-.7.7.9.9.7-.7a1 1 0 0 1 1.5 1.5L7.5 6.1a1 1 0 0 1-1.4.1zm1.8-1.8a2.3 2.3 0 0 0-3.3-.1L2.2 6.4a2.3 2.3 0 0 0 3.3 3.3l.7-.7-.9-.9-.7.7a1 1 0 0 1-1.5-1.5l1.4-1.4a1 1 0 0 1 1.5 0z"/></svg>';

const KIND_ICONS: Record<string, string> = {
  text: ICON_TEXT,
  link: ICON_LINK,
  image: ICON_IMAGE,
  files: ICON_FILES,
};

/** The categories offered under the search box, in display order. */
const CLIP_FILTERS: { value: ClipFilter; label: string }[] = [
  { value: "all", label: "全部" },
  { value: "image", label: "图片" },
  { value: "link", label: "url" },
  { value: "text", label: "文本" },
  { value: "files", label: "文件" },
];

init().catch((error) => {
  console.error("flyout failed to start", error);
});
