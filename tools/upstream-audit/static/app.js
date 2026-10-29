(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const SESSION_KEY = "kiro-upstream-audit-admin";
  const PAGE_SIZE = 50;
  const state = {
    token: "", config: null, records: [], total: 0, offset: 0,
    selectedId: null, detail: null, activeTab: "analysis", timelineLimit: 200,
    loadingRecords: false, recordsSequence: 0, detailSequence: 0,
    configDirty: false, configBusy: false, toastTimer: null,
  };
  const labels = {
    streaming: "流式", non_streaming: "非流式", thinking: "Thinking",
    tools: "工具", web_search: "网页搜索", code_execution: "代码执行",
    server_code_execution: "服务端代码执行", structured_output: "结构化输出",
    signature_replay: "签名回传", thinking_replay: "Thinking 回传",
    image: "图片", document: "文档", cache_control: "缓存断点",
    identity_probe_candidate: "身份探测", number_fingerprint_candidate: "数字指纹",
    knowledge_probe_candidate: "知识探测", models: "模型列表",
  };

  class AuthError extends Error {}

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function readSession() {
    try { return sessionStorage.getItem(SESSION_KEY) || ""; } catch { return ""; }
  }

  function storeToken(token) {
    state.token = token;
    try {
      if (token) sessionStorage.setItem(SESSION_KEY, token);
      else sessionStorage.removeItem(SESSION_KEY);
    } catch { /* Session storage can be disabled; the current tab still works. */ }
  }

  function loadBootstrapToken() {
    const fragment = new URLSearchParams(window.location.hash.slice(1));
    const token = fragment.get("admin");
    if (fragment.has("admin")) {
      // Never keep the management secret in the visible URL or browser history.
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
    }
    storeToken(token || readSession());
  }

  function toast(message, isError = false) {
    const target = $("toast");
    clearTimeout(state.toastTimer);
    target.textContent = message;
    target.classList.toggle("error", isError);
    target.hidden = false;
    state.toastTimer = setTimeout(() => { target.hidden = true; }, 3200);
  }

  function errorMessage(error) {
    return error && error.message ? error.message : "请求失败，请稍后重试。";
  }

  function setConnection(text, kind = "neutral") {
    $("connection-state").textContent = text;
    $("connection-state").className = `status-chip ${kind}`;
  }

  function showLogin(message = "") {
    storeToken("");
    state.config = null;
    state.records = [];
    state.detail = null;
    state.selectedId = null;
    state.recordsSequence += 1;
    state.detailSequence += 1;
    $("app-view").hidden = true;
    $("login-view").hidden = false;
    $("logout").hidden = true;
    $("upstream-key").value = "";
    $("relay-key").value = "";
    $("relay-key").type = "password";
    $("login-error").textContent = message;
    $("login-error").hidden = !message;
    setConnection("未登录");
  }

  async function api(path, options = {}) {
    const { raw = false, ...fetchOptions } = options;
    const headers = new Headers(fetchOptions.headers || {});
    headers.set("x-audit-admin-key", state.token);
    if (fetchOptions.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    let response;
    try {
      response = await fetch(new URL(path, document.baseURI), {
        ...fetchOptions, headers, credentials: "omit", cache: "no-store",
      });
    } catch {
      throw new Error("无法连接监控服务，请检查网络后重试。");
    }
    if (response.status === 401) {
      showLogin("管理口令无效或已失效，请重新登录。");
      throw new AuthError("请重新登录。");
    }
    if (!response.ok) {
      let message = `请求失败（HTTP ${response.status}）。`;
      try {
        const payload = await response.json();
        const detail = typeof payload.error === "string" ? payload.error : payload.error?.message;
        if (detail) message = String(detail).slice(0, 400);
        else if (typeof payload.message === "string") message = payload.message.slice(0, 400);
      } catch { /* A reverse proxy can return non-JSON errors. */ }
      throw new Error(message);
    }
    if (raw) return response;
    if (response.status === 204) return {};
    try { return await response.json(); } catch { throw new Error("监控服务返回了无法解析的数据。"); }
  }

  function relayKey(config) {
    return config?.relayKey || config?.accessKey || "";
  }

  function formatBytes(value) {
    const bytes = Number(value);
    if (!Number.isFinite(bytes)) return "—";
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  }

  function formatMs(value) {
    if (value === null || value === undefined || value === "") return "—";
    const ms = Number(value);
    if (!Number.isFinite(ms)) return "—";
    if (ms < 1000) return `${Math.round(ms)} ms`;
    return `${(ms / 1000).toFixed(2)} s`;
  }

  function asDate(value) {
    if (!value) return null;
    const input = typeof value === "number" && value < 1e12 ? value * 1000 : value;
    const date = new Date(input);
    return Number.isNaN(date.getTime()) ? null : date;
  }

  function shortTime(value) {
    const date = asDate(value);
    return date ? date.toLocaleTimeString("zh-CN", { hour12: false }) : "—";
  }

  function longTime(value) {
    const date = asDate(value);
    return date ? date.toLocaleString("zh-CN", { hour12: false }) : "—";
  }

  function normalLabels(record) {
    return (Array.isArray(record.labels) ? record.labels : []).map((item) => typeof item === "string" ? item : item?.name).filter(Boolean);
  }

  function labelText(value) { return labels[value] || value; }

  function isPending(record) {
    return ["running", "pending", "forwarding"].includes(record.state) || !record.status;
  }

  function isError(record) {
    return Number(record.status) >= 400 || ["error", "disconnected", "failed"].includes(record.state);
  }

  function stateText(record) {
    if (isPending(record)) return "进行中";
    if (record.state === "disconnected") return "客户端断开";
    if (record.state === "error" || record.state === "failed") return "错误";
    return "已完成";
  }

  function statusBadge(record) {
    const pending = isPending(record);
    const badge = element("span", `http-status${pending ? " pending" : isError(record) ? " error" : ""}`,
      pending ? "进行中" : record.status || "错误");
    badge.title = stateText(record);
    return badge;
  }

  function applyConfig(payload) {
    const config = payload.config || payload;
    state.config = config;
    $("upstream-url").value = config.upstreamBaseUrl || "";
    $("upstream-key").value = "";
    $("auth-mode").value = config.authMode || "x-api-key";
    $("enabled").checked = Boolean(config.enabled);
    $("upstream-key-state").textContent = config.hasUpstreamKey ? `已保存 ${config.upstreamKeyMasked || "••••••••"}` : "尚未保存";
    $("upstream-key").placeholder = config.hasUpstreamKey ? "留空保留已保存的上游 Key" : "粘贴对方提供的 API Key";
    $("public-url").value = config.publicBaseUrl || "";
    $("relay-key").value = relayKey(config);
    $("copy-url").disabled = !config.publicBaseUrl;
    $("copy-relay-key").disabled = !relayKey(config);
    $("toggle-relay-key").disabled = !relayKey(config);
    $("relay-key").type = "password";
    $("toggle-relay-key").textContent = "显示";
    $("toggle-relay-key").setAttribute("aria-pressed", "false");
    const ready = Boolean(config.enabled && config.upstreamBaseUrl && config.hasUpstreamKey);
    $("relay-state").textContent = ready ? "已启用" : config.upstreamBaseUrl ? "未启用" : "等待配置";
    $("relay-state").className = `status-chip ${ready ? "active" : "inactive"}`;
    setConnection(ready ? "监控已启用" : "已连接", ready ? "active" : "neutral");
    const limit = config.captureLimitBytes ? `单侧正文最多记录 ${formatBytes(config.captureLimitBytes)}` : "正文按容量上限记录";
    const retention = config.retentionHours ? `保留 ${config.retentionHours} 小时` : "定期清理";
    const records = config.recordLimit ? `最多 ${config.recordLimit} 条` : "记录数量受限";
    $("retention-info").textContent = `${limit} · ${retention} · ${records}`;
    state.configDirty = false;
    $("saved-state").textContent = "配置已同步";
  }

  function configMessage(message, kind = "") {
    $("config-message").textContent = message;
    $("config-message").className = `form-message ${kind}`;
  }

  function setConfigBusy(busy) {
    state.configBusy = busy;
    for (const id of ["save-config", "check-connection", "upstream-url", "upstream-key", "auth-mode", "enabled"]) $(id).disabled = busy;
  }

  function validatedConfig() {
    if (!$("config-form").reportValidity()) return null;
    const url = $("upstream-url").value.trim();
    let parsed;
    try { parsed = new URL(url); } catch { throw new Error("请输入完整的上游 URL，例如 https://api.example.com。"); }
    if (!["http:", "https:"].includes(parsed.protocol)) throw new Error("上游地址仅支持 HTTP 或 HTTPS。");
    if (parsed.username || parsed.password || parsed.search || parsed.hash) throw new Error("上游地址不能包含账号、密码、查询参数或 fragment，请将 Key 填在独立输入框。");
    const key = $("upstream-key").value.trim();
    if (!key && !state.config?.hasUpstreamKey) throw new Error("请填写上游 API Key。");
    return { upstreamBaseUrl: url, upstreamApiKey: key, authMode: $("auth-mode").value, enabled: $("enabled").checked };
  }

  async function saveConfiguration(silent = false) {
    const payload = validatedConfig();
    if (!payload) return false;
    const response = await api("api/config", { method: "PUT", body: JSON.stringify(payload) });
    const config = response.config || response;
    applyConfig(Object.hasOwn(config, "upstreamBaseUrl") ? config : await api("api/config"));
    if (!silent) configMessage(state.config.enabled ? "已保存，监控转发已启用。" : "已保存。启用监控转发后，再开始网站检测。", "success");
    return true;
  }

  async function loadApp() {
    $("login-button").disabled = true;
    $("login-error").hidden = true;
    try {
      const config = await api("api/config");
      applyConfig(config);
      $("admin-token").value = "";
      $("login-view").hidden = true;
      $("app-view").hidden = false;
      $("logout").hidden = false;
      await refreshRecords({ quiet: true });
    } catch (error) {
      if (!(error instanceof AuthError)) {
        $("login-view").hidden = false;
        $("login-error").textContent = errorMessage(error);
        $("login-error").hidden = false;
        setConnection("连接失败", "inactive");
      }
    } finally {
      $("login-button").disabled = false;
    }
  }

  async function copyText(text, input) {
    if (!text) return;
    try {
      if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(text);
      else {
        const temporary = element("textarea");
        temporary.value = text;
        temporary.setAttribute("readonly", "");
        temporary.className = "clipboard-helper";
        document.body.append(temporary);
        temporary.select();
        const success = document.execCommand("copy");
        temporary.remove();
        if (!success) throw new Error("Copy unavailable");
      }
      toast("已复制");
    } catch {
      if (input) { input.focus(); input.select(); }
      toast("复制失败，请选中内容后手动复制。", true);
    }
  }

  function renderFeatureOptions() {
    const select = $("feature-filter");
    const current = select.value;
    const values = new Set(state.records.flatMap(normalLabels));
    if (current && current !== "all") values.add(current);
    const options = [element("option", "", "全部特性")];
    options[0].value = "all";
    for (const value of [...values].sort()) {
      const option = element("option", "", labelText(value));
      option.value = value;
      options.push(option);
    }
    select.replaceChildren(...options);
    select.value = current || "all";
  }

  function filteredRecords() {
    const query = $("record-search").value.trim().toLocaleLowerCase();
    const status = $("status-filter").value;
    const feature = $("feature-filter").value;
    return state.records.filter((record) => {
      if (status === "pending" && !isPending(record)) return false;
      if (status === "error" && (isPending(record) || !isError(record))) return false;
      if (status === "success" && (isPending(record) || isError(record))) return false;
      const names = normalLabels(record);
      if (feature !== "all" && !names.includes(feature)) return false;
      const haystack = [record.id, record.path, record.model, record.method, record.status, ...names, ...names.map(labelText)].join(" ").toLocaleLowerCase();
      return !query || haystack.includes(query);
    });
  }

  function renderRecords() {
    const target = $("record-list");
    const records = filteredRecords();
    if (!records.length) {
      target.replaceChildren(element("p", "empty-state", state.records.length ? "当前页没有符合筛选条件的请求。" : "尚无请求。配置完成后，在 CCTest 中开始检测，记录会自动出现在这里。"));
    } else {
      const fragment = document.createDocumentFragment();
      for (const record of records) {
        const button = element("button", `record-item${record.id === state.selectedId ? " selected" : ""}`);
        button.type = "button";
        button.setAttribute("aria-pressed", String(record.id === state.selectedId));
        const top = element("div", "record-item-top");
        const path = element("span", "record-method-path");
        path.append(element("span", "method", record.method || "POST"), document.createTextNode(record.path || "/v1/messages"));
        path.title = `${record.method || "POST"} ${record.path || "/v1/messages"}`;
        top.append(path, statusBadge(record));
        const meta = element("div", "record-meta");
        meta.append(element("span", "record-model", record.model || "模型未指定"), element("span", "record-time", `${shortTime(record.startedAt)} · ${formatMs(record.durationMs)}`));
        button.append(top, meta);
        const names = normalLabels(record);
        if (names.length) {
          const row = element("div", "label-row");
          for (const name of names.slice(0, 5)) row.append(element("span", "tag", labelText(name)));
          if (names.length > 5) row.append(element("span", "tag", `+${names.length - 5}`));
          button.append(row);
        }
        button.addEventListener("click", () => selectRecord(record.id));
        fragment.append(button);
      }
      target.replaceChildren(fragment);
    }
    $("record-count").textContent = String(state.total);
    const page = Math.floor(state.offset / PAGE_SIZE) + 1;
    const pages = Math.max(1, Math.ceil(state.total / PAGE_SIZE));
    $("page-info").textContent = `第 ${page} / ${pages} 页`;
    $("previous-page").disabled = state.offset === 0;
    $("next-page").disabled = state.offset + PAGE_SIZE >= state.total;
  }

  async function refreshRecords({ quiet = false } = {}) {
    if (!state.token || state.loadingRecords) return;
    const sequence = ++state.recordsSequence;
    state.loadingRecords = true;
    $("refresh-records").disabled = true;
    try {
      const result = await api(`api/records?limit=${PAGE_SIZE}&offset=${state.offset}`);
      if (sequence !== state.recordsSequence || !state.token) return;
      const previous = state.records.find((record) => record.id === state.selectedId);
      state.records = Array.isArray(result.records) ? result.records : [];
      state.total = Number(result.total) || 0;
      if (state.offset > 0 && state.offset >= state.total) {
        state.offset = Math.max(0, Math.floor((state.total - 1) / PAGE_SIZE) * PAGE_SIZE);
        state.loadingRecords = false;
        return refreshRecords({ quiet: true });
      }
      $("records-error").hidden = true;
      $("last-updated").textContent = `更新于 ${shortTime(Date.now())} · 当前页筛选`;
      renderFeatureOptions();
      renderRecords();
      const current = state.records.find((record) => record.id === state.selectedId);
      if (current && (!state.detail || isPending(current) || isPending(state.detail) || !previous || previous.responseBytes !== current.responseBytes || previous.state !== current.state)) {
        await loadDetail(current.id, true);
      }
      if (!quiet) toast("记录已刷新");
    } catch (error) {
      if (!(error instanceof AuthError)) {
        $("records-error").textContent = errorMessage(error);
        $("records-error").hidden = false;
      }
    } finally {
      state.loadingRecords = false;
      $("refresh-records").disabled = false;
    }
  }

  async function selectRecord(id) {
    state.selectedId = id;
    state.detail = null;
    state.activeTab = "analysis";
    state.timelineLimit = 200;
    renderRecords();
    $("record-detail").replaceChildren(element("p", "empty-state", "正在读取请求详情…"));
    if (window.innerWidth <= 760) $("record-detail").scrollIntoView({ behavior: "smooth", block: "start" });
    await loadDetail(id);
  }

  async function loadDetail(id, quiet = false) {
    const sequence = ++state.detailSequence;
    try {
      const result = await api(`api/records/${encodeURIComponent(id)}`);
      if (sequence !== state.detailSequence || state.selectedId !== id || !state.token) return;
      const previous = state.detail;
      state.detail = result.record || result;
      const changed = !previous || previous.state !== state.detail.state || previous.responseBytes !== state.detail.responseBytes || previous.durationMs !== state.detail.durationMs;
      if (!quiet || changed) renderDetail();
    } catch (error) {
      if (!(error instanceof AuthError) && sequence === state.detailSequence && state.selectedId === id) {
        if (!quiet || !state.detail) $("record-detail").replaceChildren(element("p", "inline-error", errorMessage(error)));
      }
    }
  }

  function displayValue(value) {
    if (value === null || value === undefined || value === "") return "—";
    if (Array.isArray(value)) {
      const items = value.slice(0, 30).map((item) => typeof item === "object" ? JSON.stringify(item) : String(item));
      return items.join("、") + (value.length > 30 ? ` …（共 ${value.length} 项）` : "");
    }
    return typeof value === "object" ? JSON.stringify(value) : String(value);
  }

  function summaryField(title, value) {
    const wrapper = element("div", "summary-field");
    wrapper.append(element("dt", "", title), element("dd", "", displayValue(value)));
    return wrapper;
  }

  function codeView(value, extraClass = "") {
    const content = typeof value === "string" ? value : JSON.stringify(value ?? {}, null, 2);
    const pre = element("pre", `code-view ${extraClass}`, content || "（正文为空）");
    pre.tabIndex = 0;
    return pre;
  }

  function prettyBody(side) {
    const body = side?.body;
    if (typeof body !== "string") return body === undefined || body === null ? "" : JSON.stringify(body, null, 2);
    const trimmed = body.trim();
    if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
      try { return JSON.stringify(JSON.parse(trimmed), null, 2); } catch { /* SSE and partial JSON remain verbatim. */ }
    }
    return body;
  }

  function buildBodyPanel(record, sideName) {
    const side = record[sideName] || {};
    const panel = element("div", "detail-panel");
    const title = sideName === "request" ? "请求正文" : "响应正文";
    const truncated = Boolean(record[`${sideName}Truncated`] || side.truncated);
    if (truncated) panel.append(element("p", "notice", "此正文的记录已达到容量上限。监控转发仍继续，导出与本页只包含已捕获的部分。"));
    const toolbar = element("div", "body-toolbar");
    const copy = element("button", "button quiet small", "复制正文");
    copy.type = "button";
    copy.disabled = !prettyBody(side);
    copy.addEventListener("click", () => copyText(prettyBody(side)));
    toolbar.append(element("h4", "", title), copy);
    panel.append(toolbar);
    if (side.body === undefined && side.bodyBase64) panel.append(element("p", "notice", "正文仅提供二进制 Base64，请导出 JSON 查看原始字节。"));
    panel.append(codeView(prettyBody(side)));
    if (side.sha256) panel.append(element("p", "body-meta", `SHA-256：${side.sha256}`));
    panel.append(element("h4", "", "HTTP Headers（敏感值已脱敏）"), codeView(side.headers || {}, "headers-view"));
    return panel;
  }

  function buildAnalysisPanel(record) {
    const panel = element("div", "detail-panel");
    const analysis = record.analysis || {};
    const grid = element("dl", "summary-grid");
    const fields = [
      ["响应模型", analysis.responseModel], ["内容块", analysis.contentTypes],
      ["工具类型", analysis.toolTypes], ["工具 ID 前缀", analysis.toolIdPrefixes],
      ["签名片段", analysis.signatureCount ?? 0], ["签名长度（字符）", analysis.signatureLengths],
      ["原生引用数量", analysis.citationCount ?? 0], ["正文增量数量", analysis.textDeltaCount ?? 0],
      ["平台身份字段", analysis.identityPlatform], ["记录状态", stateText(record)],
    ];
    for (const [title, value] of fields) grid.append(summaryField(title, value));
    panel.append(grid);
    panel.append(element("p", "microcopy", "签名只统计长度与摘要，不执行官方密码学校验。以上是响应观察值，不能证明上游的内部实现或真实渠道。"));
    const notes = Array.isArray(analysis.notes) ? analysis.notes : analysis.notes ? [analysis.notes] : [];
    if (notes.length) {
      panel.append(element("h4", "", "观察备注"));
      const list = element("ul", "notes");
      for (const note of notes) list.append(element("li", "", displayValue(note)));
      panel.append(list);
    }
    if (record.error) panel.append(element("p", "inline-error", displayValue(record.error)));
    if (analysis.signatureHashes?.length) {
      panel.append(element("h4", "", "签名 SHA-256 摘要"), codeView(analysis.signatureHashes));
    }
    if (record.requestTruncated || record.responseTruncated) panel.append(element("p", "notice", "有正文达到捕获上限，分析摘要可能不完整。请结合截断标记判断。"));
    if (isPending(record)) panel.append(element("p", "notice", "请求仍在进行，观察值将在收到响应后更新。"));
    return panel;
  }

  function buildTimelinePanel(record) {
    const panel = element("div", "detail-panel");
    const events = Array.isArray(record.events) ? record.events : [];
    panel.append(element("p", "microcopy", "时间为监控服务收到事件时，相对本次请求开始的经过时间。同一网络块内的事件可能具有相同时间。"));
    if (!events.length) {
      panel.append(element("p", "empty-state", isPending(record) ? "等待上游 SSE 事件…" : "此响应没有可解析的 SSE 事件。原始响应仍可在“响应”页查看。"));
      return panel;
    }
    const wrapper = element("div", "timeline-wrap");
    const table = element("table", "timeline");
    const head = element("thead");
    const header = element("tr");
    for (const text of ["序号", "经过时间", "事件 / 内容类型", "索引", "字节"]) header.append(element("th", "", text));
    head.append(header);
    const body = element("tbody");
    for (const [index, event] of events.slice(0, state.timelineLimit).entries()) {
      const row = element("tr");
      const kinds = [event.type, event.blockType, event.deltaType].filter(Boolean);
      row.append(element("td", "", event.seq ?? index + 1), element("td", "timeline-time", formatMs(event.ms)), element("td", "", kinds.join(" / ") || "未识别事件"), element("td", "", event.index ?? "—"), element("td", "", event.bytes ?? "—"));
      body.append(row);
    }
    table.append(head, body);
    wrapper.append(table);
    panel.append(wrapper);
    if (events.length > state.timelineLimit) {
      const more = element("button", "button secondary small", `继续显示（已显示 ${state.timelineLimit} / ${events.length}）`);
      more.type = "button";
      more.addEventListener("click", () => { state.timelineLimit += 200; renderActivePanel(); });
      panel.append(more);
    } else panel.append(element("p", "microcopy", `共 ${events.length} 个事件`));
    return panel;
  }

  function renderActivePanel() {
    const target = $("detail-panel");
    const record = state.detail;
    if (!target || !record) return;
    for (const button of $("detail-tabs").children) button.setAttribute("aria-selected", String(button.dataset.tab === state.activeTab));
    const panel = state.activeTab === "request" || state.activeTab === "response"
      ? buildBodyPanel(record, state.activeTab)
      : state.activeTab === "timeline" ? buildTimelinePanel(record) : buildAnalysisPanel(record);
    target.replaceChildren(panel);
  }

  function renderDetail() {
    const record = state.detail;
    if (!record) return;
    const target = $("record-detail");
    const scroll = target.scrollTop;
    const title = element("div", "detail-title");
    title.append(element("h3", "", `${record.method || "POST"} ${record.path || "/v1/messages"}`), statusBadge(record));
    const id = element("p", "detail-id", record.id || "");
    const meta = element("p", "microcopy", `${longTime(record.startedAt)} · ${record.model || "模型未指定"}`);
    const metrics = element("div", "detail-metrics");
    for (const [label, value] of [["总耗时", formatMs(record.durationMs)], ["上游首字节", formatMs(record.firstByteMs)], ["请求正文", formatBytes(record.requestBytes)], ["响应正文", formatBytes(record.responseBytes)]]) {
      const metric = element("div", "metric");
      metric.append(element("small", "", label), element("strong", "", value));
      metrics.append(metric);
    }
    const tabs = element("div", "tabs");
    tabs.id = "detail-tabs";
    tabs.setAttribute("role", "tablist");
    for (const [key, label] of [["analysis", "分析摘要"], ["request", "请求"], ["response", "响应"], ["timeline", "SSE 时间线"]]) {
      const tab = element("button", "tab", label);
      tab.type = "button";
      tab.dataset.tab = key;
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-selected", String(state.activeTab === key));
      tab.setAttribute("aria-controls", "detail-panel");
      tab.addEventListener("click", () => { state.activeTab = key; renderActivePanel(); });
      tabs.append(tab);
    }
    const panel = element("div");
    panel.id = "detail-panel";
    panel.setAttribute("role", "tabpanel");
    target.replaceChildren(title, id, meta, metrics, tabs, panel);
    renderActivePanel();
    target.scrollTop = scroll;
  }

  async function exportRecords() {
    $("export-records").disabled = true;
    try {
      const response = await api("api/export", { raw: true });
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      const link = element("a");
      link.href = url;
      link.download = `api-audit-${new Date().toISOString().slice(0, 10)}.json`;
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      toast("记录已导出");
    } catch (error) {
      if (!(error instanceof AuthError)) toast(errorMessage(error), true);
    } finally { $("export-records").disabled = false; }
  }

  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const token = $("admin-token").value.trim();
    if (!token) return;
    storeToken(token);
    await loadApp();
  });
  $("logout").addEventListener("click", () => showLogin());
  $("config-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (state.configBusy) return;
    configMessage("正在保存…");
    setConfigBusy(true);
    try { await saveConfiguration(); }
    catch (error) { if (!(error instanceof AuthError)) configMessage(errorMessage(error), "error"); }
    finally { setConfigBusy(false); }
  });
  for (const id of ["upstream-url", "upstream-key", "auth-mode", "enabled"]) $(id).addEventListener("input", () => {
    state.configDirty = true;
    $("saved-state").textContent = "有未保存的修改";
    configMessage("");
  });
  $("check-connection").addEventListener("click", async () => {
    if (state.configBusy) return;
    setConfigBusy(true);
    try {
      if (state.configDirty || !state.config?.upstreamBaseUrl) {
        configMessage("正在保存配置…");
        if (!await saveConfiguration(true)) return;
      }
      configMessage("正在检查上游模型列表连接…");
      const result = await api("api/check", { method: "POST", body: "{}" });
      if (result.ok) configMessage(`连接成功（HTTP ${result.status || 200}）${result.modelCount !== undefined ? `，取得 ${result.modelCount} 个模型` : ""}。检查未调用模型生成。`, "success");
      else configMessage(`连接未通过${result.status ? `（HTTP ${result.status}）` : ""}：${displayValue(result.error)}`, "error");
    } catch (error) { if (!(error instanceof AuthError)) configMessage(errorMessage(error), "error"); }
    finally { setConfigBusy(false); }
  });
  $("copy-url").addEventListener("click", () => copyText($("public-url").value, $("public-url")));
  $("copy-relay-key").addEventListener("click", () => copyText(relayKey(state.config), $("relay-key")));
  $("toggle-relay-key").addEventListener("click", () => {
    const reveal = $("relay-key").type === "password";
    $("relay-key").type = reveal ? "text" : "password";
    $("toggle-relay-key").textContent = reveal ? "隐藏" : "显示";
    $("toggle-relay-key").setAttribute("aria-pressed", String(reveal));
  });
  $("refresh-records").addEventListener("click", () => refreshRecords());
  for (const id of ["record-search", "status-filter", "feature-filter"]) $(id).addEventListener(id === "record-search" ? "input" : "change", renderRecords);
  $("previous-page").addEventListener("click", () => { state.offset = Math.max(0, state.offset - PAGE_SIZE); refreshRecords({ quiet: true }); });
  $("next-page").addEventListener("click", () => { state.offset += PAGE_SIZE; refreshRecords({ quiet: true }); });
  $("export-records").addEventListener("click", exportRecords);
  $("clear-records").addEventListener("click", async () => {
    if (!window.confirm("确定清空已捕获的记录？如需保留，请先导出 JSON。上游配置不会被清除。")) return;
    $("clear-records").disabled = true;
    try {
      await api("api/clear", { method: "POST", body: JSON.stringify({ confirm: true }) });
      state.offset = 0;
      state.selectedId = null;
      state.detail = null;
      $("record-detail").replaceChildren(element("p", "empty-state", "记录已清空，等待新的检测请求。"));
      await refreshRecords({ quiet: true });
      toast("记录已清空");
    } catch (error) { if (!(error instanceof AuthError)) toast(errorMessage(error), true); }
    finally { $("clear-records").disabled = false; }
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && $("auto-refresh").checked && state.token) refreshRecords({ quiet: true });
  });
  setInterval(() => {
    if (!document.hidden && $("auto-refresh").checked && state.token && !$("app-view").hidden) refreshRecords({ quiet: true });
  }, 3000);

  loadBootstrapToken();
  if (state.token) loadApp();
  else showLogin();
})();
