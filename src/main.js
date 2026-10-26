const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const KIND = {
  image: "图",
  video: "视频",
  audio: "音频",
  pdf: "PDF",
  archive: "压缩",
  text: "文本",
  file: "文件",
};

const $ = (id) => document.getElementById(id);
let session = null;
let files = [];
let seen = null;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function toast(text) {
  const node = el("div", "toast", text);
  $("toasts").append(node);
  setTimeout(() => node.remove(), 2600);
}

function humanSize(bytes) {
  if (bytes < 1024) return bytes + " B";
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return (value >= 10 ? value.toFixed(0) : value.toFixed(1)) + " " + units[index];
}

function humanTime(ms) {
  const date = new Date(ms);
  const now = new Date();
  const hm = date.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
  if (date.toDateString() === now.toDateString()) {
    const diff = (Date.now() - ms) / 1000;
    if (diff < 60) return "刚刚";
    if (diff < 3600) return Math.floor(diff / 60) + " 分钟前";
    return "今天 " + hm;
  }
  return date.getMonth() + 1 + "月" + date.getDate() + "日 " + hm;
}

function setStatus(text, online) {
  const node = $("status");
  node.classList.toggle("on", online);
  node.querySelector("span").textContent = text;
}

async function writeClipboard(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch (err) {
    const area = document.createElement("textarea");
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
}

async function copyText(text) {
  await writeClipboard(text);
  toast("链接已复制");
}

const MAX_TEXT = 2 * 1024 * 1024;
let savingText = false;

function textBytes(text) {
  return new TextEncoder().encode(text).length;
}

async function submitText(text) {
  if (!text || !text.trim()) {
    toast("没有可以发送的文字");
    return;
  }
  if (textBytes(text) > MAX_TEXT) {
    toast("文字太长了，最多 2 MB");
    return;
  }
  if (savingText) return;
  savingText = true;
  try {
    const item = await invoke("save_text", { text });
    $("paste-box").value = "";
    toast("已放入 " + item.name);
    await refresh();
  } catch (err) {
    toast(String(err));
  } finally {
    savingText = false;
  }
}

async function copyFileText(file) {
  try {
    const text = await invoke("read_text_file", { name: file.name });
    await writeClipboard(text);
    toast("文字已复制，可以粘贴了");
  } catch (err) {
    toast(String(err));
  }
}

function qrMarkup(svg) {
  if (typeof svg !== "string") return "";
  const start = svg.indexOf("<svg");
  const end = svg.lastIndexOf("</svg>");
  if (start < 0 || end < start) return "";
  return svg.slice(start, end + "</svg>".length);
}

function renderSession() {
  $("sub").textContent = session.serverName + " · 窗口开着，手机才能进来";
  $("url").textContent = session.url;
  $("path").textContent = session.shareDir;
  $("no-lan").hidden = !session.noLan;
  const qr = $("qr");
  qr.replaceChildren();
  const markup = qrMarkup(session.qrSvg);
  if (markup) {
    qr.innerHTML = markup;
  } else {
    qr.append(el("p", "muted", "二维码生成失败，请复制下面的链接。"));
  }
  const more = $("more-urls");
  more.replaceChildren();
  const extras = (session.urls || []).filter((url) => url !== session.url);
  if (extras.length) {
    const box = el("div", "more");
    box.append(el("p", "muted", "如果扫码打不开，换下面的地址"));
    for (const url of extras) {
      const button = el("button", "", url);
      button.type = "button";
      button.addEventListener("click", () => copyText(url));
      box.append(button);
    }
    more.append(box);
  }
  setStatus("手机可以连接", true);
}

function renderFiles() {
  const box = $("files");
  const top = box.scrollTop;
  box.replaceChildren();
  $("count").textContent = files.length ? String(files.length) : "";
  $("clear").disabled = files.length === 0;
  if (!files.length) {
    box.append(el("p", "empty", "把文件拖进窗口，粘贴文字，或点「添加文件」。手机扫码后就能下载。"));
    return;
  }
  for (const file of files) {
    const row = el("article", "file");
    row.append(el("div", "badge " + (file.kind || "file"), KIND[file.kind] || "文件"));
    const body = el("div");
    const name = el("div", "name", file.name);
    name.title = file.name;
    const from = file.from ? "来自 " + file.from : "接收夹";
    body.append(name, el("div", "meta", humanSize(file.size) + " · " + from + " · " + humanTime(file.time)));
    const actions = el("div", "row-actions");
    if (file.kind === "text") {
      const copy = el("button", "ghost", "复制");
      copy.type = "button";
      copy.addEventListener("click", () => copyFileText(file));
      actions.append(copy);
    }
    const open = el("button", "ghost", "打开");
    open.type = "button";
    open.addEventListener("click", () => invoke("open_file", { name: file.name }).catch((err) => toast(String(err))));
    const reveal = el("button", "ghost", "所在目录");
    reveal.type = "button";
    reveal.addEventListener("click", () => invoke("reveal_file", { name: file.name }).catch((err) => toast(String(err))));
    const remove = el("button", "danger", "删除");
    remove.type = "button";
    remove.addEventListener("click", async () => {
      if (!confirm("删除「" + file.name + "」？")) return;
      try {
        await invoke("delete_file", { name: file.name });
        await refresh();
      } catch (err) {
        toast(String(err));
      }
    });
    actions.append(open, reveal, remove);
    row.append(body, actions);
    box.append(row);
  }
  box.scrollTop = top;
  document.title = files.length ? "快传 · " + files.length : "快传";
}

function applyFiles(next) {
  if (seen) {
    const old = new Set(seen);
    for (const file of next) {
      if (!old.has(file.name) && file.clientId !== "desktop") toast("收到 " + file.name);
    }
  }
  seen = next.map((file) => file.name);
  files = next;
  renderFiles();
}

async function refresh() {
  try {
    applyFiles(await invoke("list_files"));
    if (session) setStatus("手机可以连接", true);
  } catch (err) {
    setStatus("刷新失败", false);
  }
}

async function waitSession() {
  let last = "局域网服务还没起来";
  for (let i = 0; i < 80; i += 1) {
    try {
      return await invoke("session");
    } catch (err) {
      last = String(err);
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw new Error(last);
}

async function importPaths(paths) {
  if (!paths || !paths.length) return;
  try {
    const added = await invoke("import_paths", { paths });
    toast("已放入 " + added.length + " 个文件");
    await refresh();
  } catch (err) {
    toast(String(err));
  }
}

async function setupDrop() {
  const overlay = $("overlay");
  try {
    const win = window.__TAURI__.window.getCurrentWindow();
    await win.onDragDropEvent((event) => {
      const payload = event.payload || {};
      if (payload.type === "enter" || payload.type === "over") {
        overlay.hidden = false;
      } else if (payload.type === "drop") {
        overlay.hidden = true;
        importPaths(payload.paths || []);
      } else {
        overlay.hidden = true;
      }
    });
  } catch (err) {
    console.warn(err);
  }
  document.addEventListener("pointerdown", () => {
    overlay.hidden = true;
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  $("copy").addEventListener("click", () => {
    if (session) copyText(session.url);
  });
  $("send-text").addEventListener("click", () => submitText($("paste-box").value));
  $("paste-box").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      submitText($("paste-box").value);
    }
  });
  document.addEventListener("paste", (event) => {
    const target = event.target;
    if (target && target.closest && target.closest("input, textarea")) return;
    const data = event.clipboardData;
    if (!data || (data.files && data.files.length)) return;
    const pasted = data.getData("text/plain");
    if (!pasted || !pasted.trim()) return;
    event.preventDefault();
    submitText(pasted);
  });
  $("add-files").addEventListener("click", async () => {
    try {
      const added = await invoke("pick_and_import", { directory: false });
      if (added.length) toast("已放入 " + added.length + " 个文件");
      await refresh();
    } catch (err) {
      toast(String(err));
    }
  });
  $("add-folder").addEventListener("click", async () => {
    try {
      const added = await invoke("pick_and_import", { directory: true });
      if (added.length) toast("已放入 " + added.length + " 个文件");
      await refresh();
    } catch (err) {
      toast(String(err));
    }
  });
  $("open-dir").addEventListener("click", () => {
    invoke("open_share_dir").catch((err) => toast(String(err)));
  });
  $("clear").addEventListener("click", async () => {
    if (!files.length) return;
    if (!confirm("清空接收夹？桌面「快传」文件夹里的这些文件会删除。")) return;
    try {
      await invoke("clear_files");
      await refresh();
    } catch (err) {
      toast(String(err));
    }
  });

  try {
    session = await waitSession();
    renderSession();
    applyFiles(session.files || []);
  } catch (err) {
    $("sub").textContent = String(err);
    setStatus("启动失败", false);
    return;
  }
  try {
    await listen("files-changed", refresh);
  } catch (err) {
    console.warn(err);
  }
  setInterval(refresh, 2000);
  setInterval(renderFiles, 30000);
  setupDrop();
});
