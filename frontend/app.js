"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  let sequence = 0;
  const pending = new Map();
  let sessionExpired = false;
  let connectionRevision = 0;
  let reconnecting = null;
  Object.defineProperty(window, "__xuperReceive", { value(response) {
    const job = pending.get(response.id);
    if (!job) {
      if (response.ok) releasePlayback(response.data?.session_id);
      return;
    }
    pending.delete(response.id);
    clearTimeout(job.timer);
    if (response.ok) job.resolve(response.data);
    else {
      const error = new Error(response.error?.message || "La solicitud no se pudo completar.");
      error.code = response.error?.code;
      if (error.code === "session_expired") showSessionExpired(error.message);
      job.reject(error);
    }
  }});
  function call(command, params = {}) {
    if (!window.ipc?.postMessage) return Promise.reject(new Error("Abre Nébula desde el ejecutable de Windows para conectar con el catálogo."));
    return new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error("El servicio está tardando demasiado en responder. Vuelve a intentarlo."));
      }, command === "update.install" ? 300000 : command === "cast.start" ? 90000 : 60000);
      pending.set(id, { resolve, reject, timer });
      try { window.ipc.postMessage(JSON.stringify({ id, command, params })); }
      catch (error) { clearTimeout(timer); pending.delete(id); reject(error); }
    });
  }
  const state = { view: "vod", kind: "vod", category: null, categories: [], query: "", items: [], page: 1, hasMore: false, favorites: new Map(), revision: 0 };
  state.section = "home"; state.rows = [];
  const failedFeaturedArtwork=new Set();
  state.filters = {};
  let filterChoices = null, filterLoading = null;
  const filterLabels = {type:"Tipo",genre:"Género",year:"Año",country:"País de origen",audio:"Audio",score:"Puntaje"};
  const hasFilters = () => state.view === "vod" && Object.values(state.filters).some(Boolean);
  async function openFilters() {
    const dialog=$("filters-dialog"); if(!dialog.open) dialog.showModal();
    $("filters-message").textContent="Consultando los filtros disponibles…";$("filters-apply").disabled=true;
    try {
      if(!filterChoices) {
        if(!filterLoading) filterLoading=call("filters.options").finally(()=>{filterLoading=null;});
        filterChoices=await filterLoading;
      }
      $("filters-fields").replaceChildren();
      for(const [name,label] of Object.entries(filterLabels)) {
        if(!(filterChoices[name]||[]).length)continue;
        const field=node("label","filter-field");field.htmlFor=`filter-${name}`;field.append(node("span","",label));
        const select=node("select");select.id=`filter-${name}`;select.name=name;
        if(name!=="type"){const all=node("option","",name==="score"?"Cualquier puntaje":"Todos");all.value="";select.append(all);}
        for(const entry of filterChoices[name]||[]) {
          if(!entry.value) continue;
          const option=node("option","",entry.label);option.value=entry.value;select.append(option);
        }
        select.value=state.filters[name]||"";
        if(name==="type" && !select.value) {
          const pattern=state.section==="series"?/seri/i:/pel[ií]cula|film|movie/i;
          const choice=(filterChoices.type||[]).find(entry=>pattern.test(entry.label))||filterChoices.type[0];if(choice)select.value=choice.value;
        }
        select.disabled=select.options.length===1;field.append(select);$("filters-fields").append(field);
      }
      $("filters-message").textContent="Las opciones y los puntajes provienen del catálogo. Los títulos sin puntaje no se incluyen al filtrar por puntos.";
      $("filters-apply").disabled=false;
    } catch(error) {
      $("filters-fields").replaceChildren(button("Volver a consultar",openFilters,"button secondary"));
      $("filters-message").textContent=error.message;
    }
  }
  function renderFilterSummary(){
    const active=hasFilters(),host=$("active-filters");host.hidden=!active;host.replaceChildren();
    const count=Object.values(state.filters).filter(Boolean).length;
    $("filters-count").textContent=count;$("filters-count").hidden=!active;
    $("filters-open").classList.toggle("active",active);
    if(!active)return;
    for(const [name,value] of Object.entries(state.filters)) {
      if(!value)continue;const label=(filterChoices?.[name]||[]).find(entry=>entry.value===value)?.label||value;
      host.append(button(`${filterLabels[name]}: ${label}${name==="type"?"":" ×"}`,()=>{if(name==="type")openFilters();else{delete state.filters[name];loadView();}},"filter-chip"));
    }
    host.append(button("Cambiar filtros",openFilters,"filter-edit"),button("Restablecer",()=>{state.filters={};loadView();},"filter-edit"));
  }
  let detailRevision = 0;
  let playerRevision = 0;
  let hls = null;
  let activePlayback = null;
  let playerSession = null;
  let playerOptions = null;
  let playerSelection = null;
  let playerPosition = 0;
  let playerPaused = false;
  let playerBusy = false;
  let playerCleanups = [];
  let mediaRevision = 0;
  let playerControls = null;
  let historyLastSaveAt = 0;
  const historyStorageKey = "nebula.playback.history";
  const playbackHistory = readPlaybackHistory();
  const playerNotices = { source: "", audio: "" };
  const nativePlayer = window.NebulaNativeVideo?.create($("video"), call, toast);
  const nativeReady = nativePlayer?.initialize() || Promise.resolve(false);
  new ResizeObserver(entries => {
    const height = entries[0]?.borderBoxSize?.[0]?.blockSize;
    if (height) document.documentElement.style.setProperty("--featured-height", `${height}px`);
  }).observe($("featured"));
  window.addEventListener("scroll", () => document.body.classList.toggle("has-scrolled", scrollY > 20), { passive: true });
  let toastTimer;
  const key = (item) => `${item.kind || "vod"}:${item.id}`;
  function validHistoryId(id) {
    return typeof id === "string" && id.length > 0 && id.length <= 256 && id.trim() === id
      && !/[\u0000-\u001f\u007f]/.test(id) && !/^https?:\/\//i.test(id);
  }
  function nearPlaybackEnd(position, duration) {
    return duration - position <= Math.min(30, duration * 0.05);
  }
  function readPlaybackHistory() {
    const result = new Map();
    try {
      const raw = localStorage.getItem("nebula.playback.history");
      if (!raw || raw.length > 512 * 1024) return result;
      const saved = JSON.parse(raw), now = Date.now();
      if (saved?.version !== 1 || !Array.isArray(saved.entries)) return result;
      const valid = saved.entries.slice(0, 1000).filter((entry) => entry && validHistoryId(entry.id)
        && Number.isFinite(entry.position) && entry.position >= 5
        && Number.isFinite(entry.duration) && entry.duration > 0 && entry.duration <= 7 * 24 * 60 * 60
        && entry.position < entry.duration && !nearPlaybackEnd(entry.position, entry.duration)
        && Number.isFinite(entry.updatedAt) && entry.updatedAt > 0 && entry.updatedAt <= now + 60000)
        .sort((left, right) => right.updatedAt - left.updatedAt);
      for (const entry of valid) {
        if (!result.has(entry.id)) result.set(entry.id, { id: entry.id, position: entry.position, duration: entry.duration, updatedAt: entry.updatedAt });
        if (result.size === 100) break;
      }
    } catch { /* Corrupt or unavailable local storage does not interrupt playback. */ }
    return result;
  }
  function rememberedPlaybackPosition(item) {
    return item.kind === "vod" ? playbackHistory.get(item.id)?.position || 0 : 0;
  }
  function savePlaybackPosition(force = false) {
    const video = $("video"), item = activePlayback, now = Date.now();
    if (item?.kind !== "vod" || !validHistoryId(item.id) || !video.currentSrc || video.readyState < 1) return;
    if (!force && now - historyLastSaveAt < 5000) return;
    const position = video.currentTime, duration = video.duration;
    if (!Number.isFinite(position) || position < 0 || !Number.isFinite(duration) || duration <= 0 || duration > 7 * 24 * 60 * 60) return;
    historyLastSaveAt = now;
    if (video.ended || position < 5 || nearPlaybackEnd(position, duration)) playbackHistory.delete(item.id);
    else playbackHistory.set(item.id, { id: item.id, position, duration, updatedAt: now });
    const entries = [...playbackHistory.values()].sort((left, right) => right.updatedAt - left.updatedAt).slice(0, 100);
    playbackHistory.clear(); entries.forEach((entry) => playbackHistory.set(entry.id, entry));
    try { localStorage.setItem(historyStorageKey, JSON.stringify({ version: 1, entries })); }
    catch { /* In-memory resume remains available if browser storage is full. */ }
  }
  function node(tag, className, text) {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = String(text);
    return element;
  }
  function icon(name) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", `#i-${name}`);
    svg.append(use);
    return svg;
  }
  function safeUrl(value) {
    if (typeof value !== "string" || !value.trim()) return null;
    try {
      const url = new URL(value);
      return ["https:", "http:"].includes(url.protocol) && !url.username && !url.password ? url.href : null;
    } catch { return null; }
  }
  function toast(message) {
    $("toast").textContent = message;
    $("toast").hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { $("toast").hidden = true; }, 4500);
  }
  function artworkImage(url, alternate, failed) {
    const image = node("img", "art-loading"); image.alt = ""; image.decoding = "async";
    let fallback = safeUrl(alternate);
    image.addEventListener("load", () => image.classList.remove("art-loading"));
    image.addEventListener("error", () => {
      if (fallback && fallback !== image.src) { const retry = fallback; fallback = null; image.src = retry; }
      else { image.remove(); failed?.(); }
    });
    image.src = url; return image;
  }
  function button(label, action, style = "button", symbol) {
    const element = node("button", style);
    element.type = "button";
    if (symbol) element.append(icon(symbol));
    element.append(node("span", "", label));
    element.addEventListener("click", action);
    return element;
  }
  function empty(container, title, description, symbol, action, actionLabel = "Reintentar") {
    const box = node("div", "empty-state");
    const sign = node("div", "empty-symbol");
    sign.append(icon(symbol));
    box.append(sign, node("h2", "", title), node("p", "", description));
    if (action) box.append(button(actionLabel, action, "button", "refresh"));
    container.replaceChildren(box);
  }
  function loading(container, text, cards = false) {
    if (cards) {
      const grid = node("div", "skeleton-grid");
      grid.setAttribute("aria-hidden", "true");
      for (let i = 0; i < 6; i++) grid.append(node("div", "skeleton"));
      container.replaceChildren(node("p", "loading-label", text), grid);
    } else {
      const box = node("div", "empty-state");
      box.append(node("div", "spinner"), node("p", "", text));
      container.replaceChildren(box);
    }
  }
  function normalizeItem(item, fallbackKind = state.kind) {
    if (!item || item.id === undefined || item.id === null) return null;
    return { ...item, id: String(item.id), title: String(item.title || "Sin título"), kind: item.kind === "live" ? "live" : fallbackKind === "live" && !item.kind ? "live" : "vod" };
  }
  function showSessionExpired(message) {
    sessionExpired = true;
    connectionRevision++;
    $("connection").className = "connection offline";
    $("connection-label").textContent = "Sesión desconectada";
    $("connection").title = message;
    $("reconnect").hidden = false;
  }
  function applyConnectionStatus(status) {
    if (status?.error_code === "session_expired") { showSessionExpired(status.message); return; }
    sessionExpired = false;
    $("reconnect").hidden = true;
    $("connection").className = `connection ${status?.connected ? "online" : "offline"}`;
    $("connection-label").textContent = status?.connected ? "Conectado" : "Sin conexión";
    $("connection").title = status?.message || "";
    if (status?.version) $("app-version").textContent = `Nébula para Windows · ${status.version}`;
  }
  async function connectionStatus() {
    const revision = connectionRevision;
    $("connection").className = "connection checking";
    $("connection-label").textContent = "Comprobando conexión";
    try {
      const status = await call("status");
      if (revision === connectionRevision) applyConnectionStatus(status);
    } catch (error) {
      if (revision !== connectionRevision) return;
      $("connection").className = "connection offline";
      $("connection-label").textContent = "Sin conexión";
      $("connection").title = error.message;
    }
  }
  async function reconnectSession() {
    if (reconnecting) return reconnecting;
    $("reconnect").disabled = true;
    $("reconnect").textContent = "Conectando…";
    reconnecting = (async () => {
      try {
        const status = await call("session.reconnect");
        applyConnectionStatus(status);
        if (!status?.connected) throw new Error(status?.message || "No se pudo recuperar la sesión.");
        state.categories = [];
        filterChoices=null;
        await loadView();
        return true;
      } catch (error) { toast(error.message); return false; }
      finally {
        $("reconnect").disabled = false;
        $("reconnect").textContent = "Reconectar";
        reconnecting = null;
      }
    })();
    return reconnecting;
  }
  function refreshView() {
    if (sessionExpired) return reconnectSession();
    connectionStatus();
    return loadView();
  }
  async function fetchFavorites() {
    const result = await call("favorites.list");
    const items = (Array.isArray(result) ? result : result?.items)?.filter(item => item.kind !== "live");
    if (!Array.isArray(items)) throw new Error("No se pudo leer la lista de favoritos.");
    state.favorites = new Map(items.map((item) => normalizeItem(item, "vod")).filter(Boolean).map((item) => [key(item), item]));
    $("favorite-count").textContent = state.favorites.size;
    $("favorite-count").hidden = state.favorites.size === 0;
  }
  async function toggleFavorite(item, control) {
    control.disabled = true;
    try {
      await call("favorites.toggle", { id: item.id, kind: item.kind, item });
      await fetchFavorites();
      const saved = state.favorites.has(key(item));
      control.classList.toggle("saved", saved);
      control.setAttribute("aria-pressed", String(saved));
      if (control.classList.contains("card-heart")) control.setAttribute("aria-label", saved ? "Quitar de favoritos" : "Guardar en favoritos");
      if (state.view === "favorites") {
        state.items = Array.from(state.favorites.values());
        renderItems();
      } else updateFavoriteButtons();
      toast(saved ? "Guardado en favoritos" : "Quitado de favoritos");
    } catch (error) { toast(error.message); }
    finally { control.disabled = false; }
  }
  function updateFavoriteButtons() {
    document.querySelectorAll("[data-favorite-key]").forEach((element) => {
      const saved = state.favorites.has(element.dataset.favoriteKey);
      element.classList.toggle("saved", saved);
      element.setAttribute("aria-pressed", String(saved));
      if (element.classList.contains("card-heart")) element.setAttribute("aria-label", saved ? "Quitar de favoritos" : "Guardar en favoritos");
      else element.querySelector("span").textContent = element.classList.contains("hero-save") ? (saved ? "En mi lista" : "Mi lista") : (saved ? "En favoritos" : "Guardar en favoritos");
    });
  }
  function card(item) {
    const article = node("article", "card");
    const open = node("button", "card-main");
    open.type = "button";
    open.setAttribute("aria-label", `Ver ${item.title}`);
    const poster = node("div", "poster");
    const fallback = icon(item.kind === "live" ? "live" : "film");
    poster.append(fallback);
    const url = safeUrl(item.backdrop || item.poster);
    if (url) {
      const img = artworkImage(url, item.poster, () => fallback.removeAttribute("hidden"));
      img.loading = "lazy";
      fallback.setAttribute("hidden", "");
      poster.append(img);
    }
    if (item.kind === "live") poster.append(node("span", "live-badge", "EN VIVO"));
    if (Number.isFinite(item.score) && item.score>0) { const score=node("span","score-badge",`★ ${item.score.toFixed(1)}`);score.setAttribute("aria-label",`Puntaje ${item.score.toFixed(1)} de 10`);poster.append(score); }
    open.append(poster, node("h2", "card-title", item.title));
    const metadata = [item.year, item.kind === "live" ? "TV en vivo" : item.category_title].filter(Boolean).join(" · ");
    if (metadata) open.append(node("span", "card-meta", metadata));
    open.addEventListener("click", () => showDetails(item));
    const favorite = node("button", "card-heart");
    favorite.type = "button"; favorite.dataset.favoriteKey = key(item);
    favorite.append(icon("heart"));
    favorite.addEventListener("click", () => toggleFavorite(item, favorite));
    article.append(open, favorite);
    return article;
  }
  function shelf(category, items) {
    const section = node("section", "content-shelf");
    const heading = node("div", "shelf-heading"), title = node("h2", "", category.title);
    const more = button("Explorar todo ›", () => { state.category = category.id; loadView(); }, "shelf-more");
    heading.append(title, more);
    const track = node("div", "shelf-track"); track.setAttribute("aria-label", category.title);
    items.forEach(item => track.append(card(item)));
    const viewport = node("div", "shelf-viewport");
    let frame=0;
    const reduced=matchMedia("(prefers-reduced-motion: reduce)");
    const cancel=()=>{cancelAnimationFrame(frame);frame=0;track.classList.remove("shelf-moving");};
    const move=direction=>{
      cancel();const start=track.scrollLeft,target=Math.max(0,Math.min(track.scrollWidth-track.clientWidth,start+direction*track.clientWidth*.85));
      if(reduced.matches){track.scrollLeft=target;return;}
      track.classList.add("shelf-moving");const began=performance.now();
      const tick=now=>{if(!track.isConnected){cancel();return;}const t=Math.min(1,(now-began)/420),ease=1-Math.pow(1-t,3);
        track.scrollTo({left:start+(target-start)*ease,behavior:"instant"});
        if(t<1)frame=requestAnimationFrame(tick);else cancel();};frame=requestAnimationFrame(tick);
    };
    track.addEventListener("wheel",cancel,{passive:true});track.addEventListener("pointerdown",cancel,{passive:true});
    const previous = button("‹", () => move(-1), "shelf-arrow shelf-previous");
    const next = button("›", () => move(1), "shelf-arrow shelf-next");
    const updateArrows=()=>{previous.disabled=track.scrollLeft<2;next.disabled=track.scrollLeft+track.clientWidth>=track.scrollWidth-2;};
    track.addEventListener("scroll",updateArrows,{passive:true});new ResizeObserver(updateArrows).observe(track);
    previous.setAttribute("aria-label", `Ver anteriores en ${category.title}`); next.setAttribute("aria-label", `Ver siguientes en ${category.title}`);
    viewport.append(track, previous, next); section.append(heading, viewport); return section;
  }
  function renderShelves() {
    const rows = state.rows.filter(row => row.items.length);
    state.items = Array.from(new Map(rows.flatMap(row => row.items).map(item => [key(item), item])).values());
    $("results").setAttribute("aria-busy", String(!rows.length));
    $("results").classList.add("has-shelves");
    const results = $("results");
    if (results.dataset.shelfRevision !== String(state.revision)) {
      results.replaceChildren(); results.dataset.shelfRevision = String(state.revision);
    }
    const existing = new Set(Array.from(results.children, child => child.dataset.shelfKey));
    for (const row of rows) {
      const id = `${row.category.group_id || ""}:${row.category.id}`;
      if (!existing.has(id)) { const element = shelf(row.category, row.items); element.dataset.shelfKey = id; results.append(element); }
    }
    $("load-more").hidden = true; $("item-count").textContent = "";
    renderFeatured(); updateFavoriteButtons();
  }
  function categorySection(category) {
    const group=category.group_title||"",title=category.title||"";
    if(/pel[ií]cula|movie|film|cine/i.test(group))return "movies";
    if(/seri|novela|dorama/i.test(group))return "series";
    if(/pel[ií]cula|movie|film/i.test(title))return "movies";
    if(/seri/i.test(title))return "series";
    return null;
  }
  function platformName(category) {
    const title=category.title||"";
    for(const [pattern,label] of [[/netflix/i,"Netflix"],[/disney/i,"Disney+"],[/prime\s*video/i,"Prime Video"],[/\bhbo\b|\bmax\b/i,"HBO"],[/apple\s*tv/i,"Apple TV+"],[/hulu/i,"Hulu"],[/paramount/i,"Paramount+"]])if(pattern.test(title))return label;
    return null;
  }
  function categoryLabel(category) {
    const type=categorySection(category),prefix=type==="series"?"Series":type==="movies"?"Películas":"Títulos";
    if(/^2026(?:\s+movie)?$/i.test(category.title.trim()))return `${prefix} · Lo último de 2026`;
    const platform=platformName(category);if(platform)return `${prefix} de ${platform}`;
    if(/^free$/i.test(category.title.trim()))return "Selección del catálogo";
    return category.title;
  }
  function renderPlatforms() {
    const host=$("platforms");host.replaceChildren();host.hidden=true;
    if(state.view!=="vod"||state.category||hasFilters())return;
    const categories=browseCategories().filter(category=>platformName(category));
    for(const type of (state.section==="home"?["movies","series"]:[state.section])){
      const choices=categories.filter(category=>categorySection(category)===type);
      const unique=[...new Map(choices.map(category=>[platformName(category),category])).values()];if(!unique.length)continue;
      const row=node("section","platform-section"),title=node("h2","",`${type==="series"?"Series":"Películas"} por plataforma`),track=node("div","platform-options");
      for(const category of unique){const label=platformName(category),entry=button(label,()=>{state.section=type;state.category=category.id;loadView();},"platform-button");entry.setAttribute("aria-label",`${type==="series"?"Series":"Películas"} de ${label}`);track.append(entry);}
      row.append(title,track);host.append(row);host.hidden=false;
    }
  }
  function browseCategories() {
    const pattern = state.section === "series" ? /seri|novela|dorama|anime/i : state.section === "movies" ? /pel[ií]cula|movie|film|cine/i : null;
    if (!pattern) return state.categories;
    const matches = state.categories.filter(category => pattern.test(category.group_title || category.title));
    return matches.length ? matches : state.categories;
  }
  function belongsToSection(item) {
    if (!item) return false;
    if (state.view !== "vod") return true;
    // Promotional categories may mix films and series. Preserve unknown types,
    // but exclude a type the service positively identifies as the other section.
    if (state.section === "movies") return String(item.source_type) !== "0";
    if (state.section === "series") return String(item.source_type) !== "1";
    return true;
  }
  async function loadHome(revision) {
    state.rows = []; let categories = browseCategories();
    renderPlatforms();
    const rank=category=>/^2026(?:\s+movie)?$/i.test(category.title.trim())?0:platformName(category)?1:/^free$|trailer|^all$|^todos$|^todas$/i.test(category.title.trim())?3:2;
    const chosen=[],seen=new Set();
    for(const category of [...categories].sort((a,b)=>rank(a)-rank(b))){
      const identity=rank(category)<2?`${categorySection(category)}:${platformName(category)||"2026"}`:String(category.id);
      if(seen.has(identity))continue;seen.add(identity);chosen.push(category);if(chosen.length===(state.section==="home"?12:10))break;
    }
    let failures = 0;
    for (const category of chosen) {
      if (revision !== state.revision) return;
      try {
        const page = await call("catalog", { kind: "vod", category: category.id, page: 1 });
        if (revision !== state.revision) return;
        const items = (page.items || []).map(item => normalizeItem(item)).filter(belongsToSection);
        state.rows.push({ category:{...category,title:categoryLabel(category)}, items }); renderShelves();
      } catch (error) { failures++; if (sessionExpired) throw error; }
    }
    if (revision !== state.revision) return;
    if (!state.items.length) throw new Error(failures ? "No pudimos cargar las filas del catálogo. Vuelve a intentarlo." : "El catálogo no devolvió títulos para esta sección.");
    $("results").setAttribute("aria-busy", "false");
    $("load-more").disabled = false;
    if (failures) { $("result-notice").textContent = "Algunas filas no se pudieron cargar. Puedes actualizar para intentarlo otra vez."; $("result-notice").hidden = false; }
  }
  function renderItems() {
    $("results").classList.remove("has-shelves");
    $("results").setAttribute("aria-busy", "false");
    $("item-count").textContent = state.items.length ? `${state.items.length} ${state.items.length === 1 ? "título" : "títulos"}` : "";
    $("load-more").hidden = !state.hasMore || state.view === "favorites";
    renderFeatured();
    if (!state.items.length) {
      if(hasFilters()) empty($("results"),"No hay títulos con estos filtros","Prueba otro género, año, país, audio o puntaje.","search",openFilters,"Cambiar filtros");
      else if (state.view === "favorites") empty($("results"), "Tus favoritos, aquí", "Guarda un título con el corazón para encontrarlo fácilmente después.", "heart");
      else if (state.view === "search") empty($("results"), "No encontramos coincidencias", "Prueba con otro título de película o serie.", "search");
      else empty($("results"), "No hay contenido disponible", "El servicio no devolvió títulos para esta selección.", state.kind === "live" ? "live" : "film", () => loadView());
      return;
    }
    const grid = node("div", `grid${state.kind === "live" && state.view !== "favorites" ? " live-grid" : ""}`);
    state.items.forEach((item) => grid.append(card(item)));
    $("results").replaceChildren(grid);
    updateFavoriteButtons();
  }
  function renderFeatured() {
    const featured = $("featured");
    const item = state.view === "vod" && !state.category && !hasFilters() ? (state.items.find(entry=>!failedFeaturedArtwork.has(key(entry))&&safeUrl(entry.backdrop||entry.poster))||state.items[0]) : null;
    featured.hidden = !item;
    document.body.classList.toggle("has-featured", Boolean(item));
    if (item && featured.dataset.itemKey === key(item)) return;
    featured.replaceChildren(); delete featured.dataset.itemKey;
    if (!item) return;
    featured.dataset.itemKey = key(item);
    const url = safeUrl(item.backdrop || item.poster);
    if (url) {
      const art = node("div", "hero-art");
      const ambient=artworkImage(url,item.poster);ambient.classList.add("hero-ambient");ambient.setAttribute("aria-hidden","true");
      const backdrop = artworkImage(url, item.poster, () => {
        art.remove();if(failedFeaturedArtwork.size<200)failedFeaturedArtwork.add(key(item));
        if(featured.dataset.itemKey===key(item))renderFeatured();
      });
      backdrop.classList.add("hero-image");
      backdrop.fetchPriority = "high";
      art.append(ambient,backdrop);
      featured.append(art);
    }
    const content = node("div", "hero-content");
    const selected = state.categories.find(category => String(category.id) === String(state.category));
    content.append(node("p", "hero-kicker", "N É B U L A  ·  SELECCIÓN"), node("h2", "hero-title", item.title));
    content.append(node("p", "hero-meta", [selected?.group_title, selected?.title].filter(Boolean).join(" · ") || "En el catálogo de Nébula"));
    if(Number.isFinite(item.score)&&item.score>0) content.append(node("p","hero-score",`★ ${item.score.toFixed(1)} / 10`));
    content.append(node("p", "hero-description", item.description || "Tu próxima historia empieza aquí. Consulta los detalles de este título y elige qué ver."));
    const actions = node("div", "hero-actions");
    actions.append(button("Reproducir", () => playFromCard(item), "button primary", "play"));
    actions.append(button("Más información", () => showDetails(item), "button secondary", "info"));
    actions.append(window.NebulaReleaseUI.shareButton(item));
    const favorite = button("Mi lista", () => toggleFavorite(item, favorite), "button secondary hero-save", "heart");
    favorite.dataset.favoriteKey = key(item);
    content.append(actions);
    featured.append(content);
  }
  async function playFromCard(item) {
    try {
      const details = await call("details", { id: item.id, kind: item.kind });
      const full = normalizeItem({ ...item, ...details }, item.kind);
      const episodes = episodeItems(full);
      const remembered = [...episodes].reverse().find(episode => rememberedPlaybackPosition(episode) > 0);
      showPlayer(remembered || episodes[0] || full);
    } catch (error) { toast(error.message); }
  }
  async function loadCategories(revision) {
    $("categories").replaceChildren(); $("category-error").hidden = true;
    if(hasFilters())return;
    if (!["vod", "live"].includes(state.view)) return;
    try {
      const result = await call("categories", { kind: state.kind });
      if (revision !== state.revision) return;
      const categories = Array.isArray(result) ? result : result?.items;
      if (!Array.isArray(categories)) throw new Error("Formato de categorías inesperado.");
      state.categories = categories;
      if (state.view === "vod") {
        if (state.category && !categories.some(category => String(category.id) === String(state.category))) state.category = null;
        const field = node("label", "genre-picker"); field.htmlFor = "category-shelf";
        field.append(node("span", "", "Géneros"));
        const select = node("select", "category-select"); select.id = "category-shelf";
        const all = node("option", "", "Todos los géneros"); all.value = ""; select.append(all);
        browseCategories().forEach(category => {
          const option = node("option", "", categoryLabel(category)); option.value = category.id; select.append(option);
        });
        select.value = state.category == null ? "" : String(state.category);
        select.addEventListener("change", () => { state.category = select.value || null; loadView(); });
        field.append(select); $("categories").append(field); return;
      }
      if (!categories.some(category => String(category.id) === String(state.category))) state.category = categories[0]?.id;
      categories.forEach(category => {
        const chip = button(category.title || "Sin categoría", () => { state.category = category.id; loadView(); }, `chip${String(category.id) === String(state.category) ? " selected" : ""}`);
        chip.setAttribute("aria-pressed", String(String(category.id) === String(state.category))); $("categories").append(chip);
      });
    } catch (error) {
      if (revision !== state.revision) return;
      $("category-error").textContent = error.message; $("category-error").hidden = false; return error;
    }
  }
  function heading() {
    const titles = { vod: ["EXPLORA NÉBULA", "Encuentra tu próxima historia", "Películas, series y mucho más."], live: ["AHORA EN DIRECTO", "TV en vivo", "Tus canales, en un solo lugar."], search: ["ENCUENTRA TU PRÓXIMO TÍTULO", "Buscar", state.query ? `Resultados para «${state.query}»` : "Escribe un título para empezar."], favorites: ["GUARDADO PARA TI", "Mi lista", "Lo que quieres ver, siempre a mano."] };
    if (state.view === "vod" && state.section !== "home") titles.vod = ["", state.section === "series" ? "Series" : "Películas", ""];
    if(hasFilters()) titles.vod=["", "Explorar", ""];
    document.body.classList.toggle("home-view", state.view === "vod" && state.section === "home" && !state.category && !hasFilters());
    renderFilterSummary();
    const [eyebrow, title, subtitle] = titles[state.view];
    $("eyebrow").textContent = eyebrow; $("page-title").textContent = title; $("page-subtitle").textContent = subtitle;
    $("search-input").placeholder = "Buscar en el catálogo";
    document.querySelectorAll(".nav-item").forEach((element) => {
      const selected = element.dataset.view === (state.view === "vod" && state.section !== "home" ? state.section : state.view);
      element.classList.toggle("active", selected);
      if (selected) element.setAttribute("aria-current", "page"); else element.removeAttribute("aria-current");
    });
    document.querySelectorAll("[data-kind]").forEach((element) => {
      element.classList.toggle("selected", element.dataset.kind === state.kind);
      element.setAttribute("aria-pressed", String(element.dataset.kind === state.kind));
    });
  }
  async function loadView(more = false) {
    if(!more)$("platforms").hidden=true;
    const revision = ++state.revision;
    heading();
    $("load-more").disabled = true;
    if (!more) {
      window.scrollTo({ top: 0, behavior: "instant" });
      state.items = []; state.page = 1; state.hasMore = false;
      renderFeatured();
      $("item-count").textContent = ""; $("load-more").hidden = true;
      $("result-notice").hidden = true;
      $("results").setAttribute("aria-busy", "true");
      loading($("results"), state.view === "favorites" ? "Cargando tus favoritos…" : "Consultando el catálogo…", true);
      const categoryError = await loadCategories(revision);
      if (revision !== state.revision) return;
      if (categoryError) {
        $("results").setAttribute("aria-busy", "false");
        $("category-error").hidden = true;
        empty($("results"), "No pudimos cargar el contenido", categoryError.message, "connection", refreshView, sessionExpired ? "Reconectar" : "Reintentar");
        $("load-more").disabled = false;
        return;
      }
    }
    if (state.view === "search" && !state.query) {
      $("results").setAttribute("aria-busy", "false");
      empty($("results"), "Encuentra algo que te guste", "Escribe el título en la barra de búsqueda y pulsa Enter.", "search");
      $("load-more").disabled = false;
      return;
    }
    try {
      if (state.view === "vod" && !state.category && !hasFilters()) { await loadHome(revision); return; }
      if (state.view === "favorites") {
        await fetchFavorites();
        if (revision !== state.revision) return;
        state.items = Array.from(state.favorites.values());
      } else {
        const page = more ? state.page + 1 : 1;
        const data = await call(hasFilters()?"filters.catalog":state.view === "search" ? "search" : "catalog", { kind: state.kind, category: state.category, query: state.query, filters:state.filters, page });
        if (revision !== state.revision) return;
        if (!data || !Array.isArray(data.items)) throw new Error("El servicio devolvió un formato de catálogo inesperado.");
        const incoming = data.items.map((item) => normalizeItem(item)).filter(belongsToSection);
        state.items = Array.from(new Map([...state.items, ...incoming].map((item) => [key(item), item])).values());
        state.page = data.page || page; state.hasMore = data.has_more === true;
        $("result-notice").textContent = typeof data.notice === "string" ? data.notice : "";
        $("result-notice").hidden = !$("result-notice").textContent;
      }
      renderItems();
    } catch (error) {
      if (revision !== state.revision) return;
      $("results").setAttribute("aria-busy", "false");
      if (more) toast(error.message);
      else {
        $("category-error").hidden = true;
        $("result-notice").hidden = true;
        empty($("results"), state.view === "favorites" ? "No pudimos leer tus favoritos" : "No pudimos cargar el contenido", error.message, "connection", refreshView, sessionExpired ? "Reconectar" : "Reintentar");
      }
    } finally { if (revision === state.revision) $("load-more").disabled = false; }
  }
  function navigate(view) {
    if (!["vod", "series", "movies", "search", "favorites"].includes(view)) return;
    state.kind = "vod";
    state.filters={};
    state.section = view === "series" || view === "movies" ? view : "home";
    if (view === "series" || view === "movies") view = "vod";
    state.view = view; state.category = null;
    if (view === "vod" || view === "live") { state.kind = view; state.query = ""; $("search-input").value = ""; }
    if (view === "search") { state.query = $("search-input").value.trim(); $("search-input").focus(); }
    loadView();
  }
  async function showDetails(item) {
    const revision = ++detailRevision, dialog = $("details-dialog");
    loading($("details-content"), "Cargando detalles…");
    if (!dialog.open) dialog.showModal();
    dialog.scrollTop = 0;
    try {
      const details = await call("details", { id: item.id, kind: item.kind });
      if (revision !== detailRevision || !dialog.open) return;
      const full = normalizeItem({ ...item, ...details }, item.kind);
      let current = full, seasonRevision = 0;
      const seasons = Array.isArray(full.seasons) ? full.seasons : [];
      const layout = node("div", "details-cinema"), hero = node("div", "details-hero");
      const art = safeUrl(full.backdrop || full.poster);
      if (art) { const image = artworkImage(art, full.poster); image.classList.add("details-art"); hero.append(image); }
      const heroBody = node("div", "details-hero-body");
      heroBody.append(node("div", "details-brand", "N · NÉBULA"));
      const title = node("h2", "", full.title); title.id = "details-title"; heroBody.append(title);
      const actions = node("div", "details-actions");
      const play = button("Reproducir", () => {
        const episodes = episodeItems(current), remembered = [...episodes].reverse().find(episode => rememberedPlaybackPosition(episode) > 0);
        showPlayer(remembered || episodes[0] || current);
      }, "button primary", "play");
      const favorite = button("Mi lista", () => toggleFavorite(full, favorite), "button secondary detail-save", "heart"); favorite.dataset.favoriteKey = key(full);
      const castButton = button("Enviar al televisor", () => {
        const first = episodeItems(current)[0] || current; openCast(first, castButton);
      }, "icon-button detail-cast", "cast");
      castButton.setAttribute("aria-label","Enviar al televisor");castButton.title="Enviar al televisor con Chromecast";
      actions.append(play, favorite, castButton, window.NebulaReleaseUI.shareButton(full)); heroBody.append(actions); hero.append(heroBody); layout.append(hero);
      const body = node("div", "details-body"), overview = node("div", "details-overview");
      const summary = node("div", "details-summary");
      summary.append(node("div", "details-meta", [full.year, full.source_type === "0" ? "Serie" : full.kind === "live" ? "En directo" : "Película"].filter(Boolean).join("   ·   ")));
      if(Number.isFinite(full.score)&&full.score>0) summary.append(node("p","details-score",`★ ${full.score.toFixed(1)} / 10`));
      summary.append(node("p", "details-description", full.description || "No hay una descripción disponible para este título."));
      const about = node("div", "details-about");
      if (full.cast) { const cast = node("p"); cast.append(node("span", "", "Reparto: "), document.createTextNode(full.cast)); about.append(cast); }
      if (full.genres) { const genres = node("p"); genres.append(node("span", "", "Géneros: "), document.createTextNode(full.genres)); about.append(genres); }
      for(const [name,label] of [["director","Dirección"],["country","País"],["language","Idioma original"],["duration","Duración"]]){
        if(!full[name])continue;const row=node("p");row.append(node("span","",`${label}: `),document.createTextNode(full[name]));about.append(row);
      }
      overview.append(summary, about); body.append(overview);
      const isSeries = full.source_type === "0" || seasons.length > 0 || full.episodes?.length > 1;
      if (isSeries) {
        const section = node("section", "details-episodes"), heading = node("div", "episodes-heading");
        heading.append(node("h3", "", "Episodios"));
        const selector = node("select", "season-select"); selector.id = "season-select"; selector.setAttribute("aria-label", "Temporada");
        const options = seasons.length ? [...seasons].sort((a, b) => a.number - b.number) : [{ id: full.id, number: null }];
        options.forEach(season => { const option = node("option", "", season.number == null ? "Episodios disponibles" : `Temporada ${season.number}`); option.value = String(season.id); selector.append(option); });
        selector.value = String(full.id); if (!selector.value) selector.selectedIndex = 0;
        selector.hidden = options.length < 2; heading.append(selector);
        const list = node("div", "season-episodes"); list.id = "season-episodes";
        function renderEpisodes() {
          const episodes = episodeItems(current);
          list.replaceChildren(); list.setAttribute("aria-busy", "false");
          const count = node("p", "episode-count", `${episodes.length} ${episodes.length === 1 ? "episodio" : "episodios"}`); list.append(count);
          episodes.forEach((episode, index) => {
            const row = node("button", "episode"); row.type = "button";
            row.setAttribute("aria-label", `Reproducir ${episode.title}`);
            row.append(node("span", "episode-number", String(episode.number ?? index + 1)));
            const thumb = node("div", "episode-thumb"), url = safeUrl(episode.poster || current.backdrop || full.backdrop || full.poster);
            if (url) { const image = artworkImage(url, episode.poster || full.poster); image.loading = "lazy"; thumb.append(image); }
            thumb.append(icon("play")); row.append(thumb);
            const text = node("div", "episode-copy"); text.append(node("h4", "", episode.title));
            if (episode.duration) text.append(node("span", "episode-duration", episode.duration));
            if (rememberedPlaybackPosition(episode) > 0) text.append(node("span", "episode-resume", `Continuar desde ${Math.floor(rememberedPlaybackPosition(episode) / 60)} min`));
            row.append(text); row.addEventListener("click", () => showPlayer(episode)); list.append(row);
          });
          if (!episodes.length) list.append(node("p", "inline-note", "Esta temporada no tiene episodios disponibles."));
        }
        selector.addEventListener("change", async () => {
          const selected = selector.value, request = ++seasonRevision;
          selector.disabled = true; play.disabled = true; list.setAttribute("aria-busy", "true"); loading(list, "Cargando temporada…");
          try {
            const data = await call("details", { id: selected, kind: "vod", season_switch: true });
            if (request !== seasonRevision || revision !== detailRevision || !dialog.open) return;
            current = normalizeItem({ ...full, ...data, seasons, series_title: full.title, season_id: selected }, "vod");
            title.textContent = current.title;
            renderEpisodes();
          } catch (error) {
            if (request !== seasonRevision || revision !== detailRevision || !dialog.open) return;
            selector.value = current.id; renderEpisodes(); toast(error.message);
          } finally { if (request === seasonRevision) { selector.disabled = false; play.disabled = false; } }
        });
        renderEpisodes(); section.append(heading, list); body.append(section);
      }
      layout.append(body); $("details-content").replaceChildren(layout); updateFavoriteButtons();
    } catch (error) {
      if (revision !== detailRevision || !dialog.open) return;
      empty($("details-content"), "No se pudieron cargar los detalles", error.message, "connection", () => showDetails(item));
    }
  }
  function releasePlayback(sessionId) {
    if (typeof sessionId === "string" && /^[a-f0-9]{32}$/i.test(sessionId)) {
      call("playback.stop", { session_id: sessionId }).catch(() => {});
    }
  }
  async function openCast(item, control) {
    if (!item || control.disabled) return;
    control.disabled = true;
    try {
      const sameItem = activePlayback && key(activePlayback) === key(item);
      const current = sameItem ? $("video").currentTime : 0;
      const preferences = sameItem && item.kind === "vod" ? playbackPreferences() : {};
      window.NebulaCast.open({ id: item.id, kind: item.kind, title: item.title, start_time: Number.isFinite(current) ? current : 0, ...preferences },()=>{
        if(activePlayback&&key(activePlayback)===key(item))$("video").pause();
      });
    } catch (error) {
      if ($("player-dialog").open) playerMessage(error.message, true);
      else toast(error.message);
    } finally { control.disabled = false; }
  }
  function stopVideo() {
    savePlaybackPosition(true);
    mediaRevision++;
    playerControls?.sourceStopped();
    playerCleanups.splice(0).forEach((clean) => clean());
    if (hls) { hls.destroy(); hls = null; }
    const video = $("video");
    if (video.currentSrc && Number.isFinite(video.currentTime)) playerPosition = video.currentTime;
    video.pause(); nativePlayer?.stop(); video.removeAttribute("src"); video.load();
    releasePlayback(playerSession);
    playerSession = null;
  }
  function playerMessage(message, error = false) {
    $("player-message").textContent = message;
    $("player-message").parentElement.classList.toggle("error", error);
    $("player-retry").hidden = !error;
  }
  function renderPlayerNotices() {
    const message = [playerNotices.source, playerNotices.audio].filter(Boolean).join(" ");
    $("player-settings-note").textContent = message;
    $("player-settings-note").hidden = !message;
  }
  function playbackPreferences(selection = playerSelection) {
    const value = {};
    if (typeof selection?.quality === "string" && selection.quality) value.quality = selection.quality;
    if (typeof selection?.audio_language === "string" && selection.audio_language) value.audio_language = selection.audio_language;
    if (selection === playerSelection && nativePlayer?.active) {
      const track = nativePlayer.audioTracks.find(track => track.id === nativePlayer.selectedAudio);
      const quality = playerOptions?.qualities.find(quality => quality.value === selection?.quality);
      const language = quality?.audio_languages.find(language => languageCode(language.value) === languageCode(track?.language));
      if (language) value.audio_language = language.value;
    }
    return value;
  }
  function rememberedPlaybackSelection(options) {
    let remembered;
    try { remembered = JSON.parse(localStorage.getItem("nebula.playback.preferences") || "null"); } catch { return options.selected; }
    if (!remembered || !options.qualities.length) return options.selected;
    const quality = options.qualities.find((entry) => entry.value === remembered.quality)
      || options.qualities.find((entry) => entry.value === options.selected?.quality) || options.qualities[0];
    const language = quality.audio_languages.find((entry) => entry.value === remembered.audio_language)
      || quality.audio_languages.find((entry) => entry.value === options.selected?.audio_language) || quality.audio_languages[0];
    if (quality.value === options.selected?.quality && (language?.value || "") === (options.selected?.audio_language || "")) return options.selected;
    return { quality: quality.value, ...(language ? { audio_language: language.value } : {}) };
  }
  function parsePlaybackOptions(data) {
    if (!data || !Array.isArray(data.qualities)) throw new Error("No se pudieron leer las opciones de reproducción.");
    const qualities = data.qualities.filter((quality) => quality && typeof quality.value === "string" && quality.value)
      .map((quality) => ({ value: quality.value, label: String(quality.label || quality.value),
        audio_languages: (Array.isArray(quality.audio_languages) ? quality.audio_languages : [])
          .filter((language) => language && typeof language.value === "string" && language.value)
          .map((language) => ({ value: language.value, label: String(language.label || language.value) })) }));
    return { qualities, selected: data.selected || null, notice: typeof data.notice === "string" ? data.notice : "" };
  }
  function renderPlayerOptions() {
    const qualities = playerOptions?.qualities || [];
    const quality = qualities.find((value) => value.value === playerSelection?.quality);
    const languages = quality?.audio_languages || [];
    function fill(select, entries, selected) {
      select.replaceChildren(...entries.map((entry) => {
        const option = node("option", "", entry.label);
        option.value = entry.value;
        option.selected = entry.value === selected;
        return option;
      }));
      select.value = selected || "";
      select.disabled = playerBusy || entries.length < 2;
    }
    fill($("player-quality"), qualities, quality?.value);
    playerControls?.setQualityOptions(qualities, quality, playerOptions?.notice);
    const nativeTracks = nativePlayer?.active ? nativePlayer.audioTracks : [];
    const actualAudio = nativeTracks.map((track, index) => ({ value: `vlc:${track.id}`, label: audioLabel(track.language, track.label, index) }));
    const labelCounts = new Map();
    actualAudio.forEach(entry => labelCounts.set(entry.label,(labelCounts.get(entry.label)||0)+1));
    const occurrences = new Map();
    for (const entry of actualAudio) {
      const label = entry.label;
      if (labelCounts.get(label) < 2) continue;
      const ordinal = (occurrences.get(label) || 0) + 1; occurrences.set(label, ordinal);
      entry.label = `${label} (pista ${ordinal})`;
    }
    if (actualAudio.length) {
      for (const language of languages) {
        if (!nativeTracks.some(track => languageCode(track.language) === languageCode(language.value))) actualAudio.push(language);
      }
    }
    fill($("player-audio"), actualAudio.length ? actualAudio : languages, actualAudio.length ? `vlc:${nativePlayer.selectedAudio}` : playerSelection?.audio_language);
    $("player-settings").hidden = !qualities.length && !actualAudio.length;
    $("player-quality-field").hidden = !qualities.length;
    $("player-audio-field").hidden = !languages.length && !actualAudio.length;
    renderPlayerNotices();
  }
  function audioLabel(language, fallback, index) {
    const code = languageCode(language);
    try {
      const name = code ? new Intl.DisplayNames(["es"], { type: "language" }).of(code) : "";
      return name ? name.charAt(0).toUpperCase() + name.slice(1) : fallback || `Pista ${index + 1}`;
    } catch { return fallback || `Pista ${index + 1}`; }
  }
  function languageCode(value) {
    if (typeof value !== "string" || !value.trim()) return "";
    const aliases = { spa:"es", eng:"en", por:"pt", kor:"ko", jpn:"ja", fre:"fr", fra:"fr", ger:"de", deu:"de", ita:"it", rus:"ru", chi:"zh", zho:"zh", und:"" };
    value = value.trim().toLowerCase().replace(/_/g,"-");
    const base=value.split("-")[0];
    if (Object.hasOwn(aliases,base)) return aliases[base];
    try { return new Intl.Locale(value.trim().replace(/_/g, "-")).language.toLowerCase(); }
    catch { return value.trim().toLowerCase().split("-")[0]; }
  }
  function audioTrackIndex(tracks, selection) {
    const wanted = languageCode(selection?.audio_language);
    const languages = tracks.map((track) => languageCode(track.lang || track.language || track.attrs?.LANGUAGE));
    const matching = wanted ? languages.findIndex((language) => language === wanted) : -1;
    if (matching >= 0) return matching;
    // A manifest's audio order can differ from the source bundle. Prefer its
    // declared language; use the APK's index only when track languages are absent.
    if (wanted && languages.some(Boolean)) return -1;
    const index = selection?.audio_track_index;
    return Number.isInteger(index) && index >= 0 && index < tracks.length ? index : -1;
  }
  function applyPlayerAudio(video, engine, settled = false) {
    let selection = playerSelection;
    if (!nativePlayer?.active && (!selection || (!selection.audio_language && selection.audio_track_index == null))) return;
    selection = selection || {};
    const nativeTracks = video.audioTracks;
    const tracks = engine?.audioTracks?.length ? Array.from(engine.audioTracks) : nativeTracks?.length ? Array.from(nativeTracks) : [];
    if (nativePlayer?.active) {
      try {
        const preferred=localStorage.getItem("nebula.audio.language");
        if (preferred && tracks.some(track => languageCode(track.language) === preferred)) selection={...selection,audio_language:preferred};
      } catch {}
    }
    const index = audioTrackIndex(tracks, selection);
    if (nativePlayer?.active && index >= 0) {
      const track = nativePlayer.audioTracks[index], revision = mediaRevision;
      playerNotices.audio = "";
      renderPlayerNotices();
      if (track && nativePlayer.selectedAudio !== track.id) {
        nativePlayer.selectAudio(track.id).then(success => {
          if (revision !== mediaRevision || !nativePlayer.active) return;
          playerNotices.audio = success ? "" : "El reproductor no pudo aplicar la pista de audio seleccionada.";
          renderPlayerNotices();
        });
      }
      return;
    }
    let warning = "";
    if (index >= 0) {
      try {
        if (engine?.audioTracks?.length) {
          if (engine.audioTrack !== index) engine.audioTrack = index;
        } else {
          if (!tracks[index].enabled) tracks[index].enabled = true;
          tracks.forEach((track, trackIndex) => { if (trackIndex !== index && track.enabled) track.enabled = false; });
          if (!tracks[index].enabled) throw new Error("Audio track not enabled");
        }
      } catch { warning = "El reproductor no pudo aplicar la pista de audio seleccionada."; }
    } else if (settled && (tracks.length || Number(selection.audio_track_index) > 0)) {
      warning = tracks.length
        ? "El reproductor no encontró la pista del idioma solicitado; se conserva el audio predeterminado de la fuente."
        : "Esta fuente ofrece varias pistas, pero el reproductor de Windows no permite elegirlas. Se conserva el audio predeterminado; el idioma solicitado no se pudo aplicar.";
    }
    playerNotices.audio = warning;
    renderPlayerNotices();
  }
  function listenPlayer(target, event, handler) {
    target.addEventListener(event, handler);
    playerCleanups.push(() => target.removeEventListener(event, handler));
  }
  async function showPlayer(item, requestedPreferences) {
    $("details-dialog").close();
    const sameItem = activePlayback && key(activePlayback) === key(item);
    const preferences = requestedPreferences || (sameItem ? playbackPreferences() : {});
    const video = $("video");
    const paused = sameItem ? playerPaused : false;
    const revision = ++playerRevision;
    stopVideo(); activePlayback = item;
    if (!sameItem) { playerPosition = rememberedPlaybackPosition(item); playerOptions = null; playerSelection = null; }
    historyLastSaveAt = 0;
    const resumeTime = item.kind === "live" ? 0 : playerPosition;
    playerPaused = paused;
    playerBusy = true;
    playerControls?.setContext(item, episodeItems(item));
    playerControls?.setBusy(true);
    playerNotices.source = ""; playerNotices.audio = "";
    renderPlayerOptions();
    $("player-title").textContent = item.title;
    playerMessage(item.kind === "live" ? "Consultando la fuente de reproducción…" : "Consultando calidad e idiomas…");
    if (!$("player-dialog").open) $("player-dialog").showModal();
    try {
      if (item.kind !== "live") {
        const data = await call("playback.options", { id: item.id, kind: item.kind, ...preferences });
        if (revision !== playerRevision || !$("player-dialog").open) return;
        playerOptions = parsePlaybackOptions(data);
        playerSelection = !sameItem && !requestedPreferences ? rememberedPlaybackSelection(playerOptions) : playerOptions.selected;
        playerNotices.source = playerOptions.notice;
        renderPlayerOptions();
      } else { playerOptions = null; playerSelection = null; renderPlayerOptions(); }
      playerMessage("Consultando la fuente de reproducción…");
      const source = await call("playback", { id: item.id, kind: item.kind, ...playbackPreferences() });
      if (revision !== playerRevision || !$("player-dialog").open) {
        releasePlayback(source?.session_id);
        return;
      }
      playerSession = source?.session_id || null;
      if (source?.selection) { playerSelection = source.selection; renderPlayerOptions(); }
      if (requestedPreferences) {
        try { localStorage.setItem("nebula.playback.preferences", JSON.stringify(playbackPreferences())); } catch { /* Keep the current selection in memory. */ }
      }
      const url = safeUrl(source?.url);
      if (!url) throw new Error("Este título no tiene una fuente de video disponible en este momento.");
      $("player-title").textContent = source.title || item.title;
      const headers = source.headers && typeof source.headers === "object" ? Object.entries(source.headers) : [];
      if (headers.some(([name]) => /^(user-agent|referer|cookie|host|origin|content-length|sec-|proxy-)/i.test(name))) throw new Error("Esta fuente de video todavía no es compatible con la aplicación de Windows.");
      const mime = String(source.mime_type || "").toLowerCase();
      const isHls = /mpegurl/.test(mime) || /\.m3u8(?:$|[?#])/i.test(url);
      const mediaGeneration = mediaRevision;
      const current = () => revision === playerRevision && mediaGeneration === mediaRevision && $("player-dialog").open;
      const fail = (message) => { if (current()) { playerMessage(message, true); stopVideo(); } };
      let resumeApplied = resumeTime <= 0;
      function resumePosition() {
        if (!current() || resumeApplied || video.readyState < 1) return;
        if (nativePlayer?.active && (!video.seekable.length || !nativePlayer.canSeek)) return;
        const position = Number.isFinite(video.duration) ? Math.min(resumeTime, Math.max(0, video.duration - 0.1)) : resumeTime;
        try { video.currentTime = position; resumeApplied = true; playerPosition = position; } catch { /* Try again when the media becomes seekable. */ }
      }
      function beginPlayback() {
        if (!current()) return;
        resumePosition();
        if (paused) { video.pause(); playerMessage("En pausa"); return; }
        video.play().catch((error) => {
          if (!current()) return;
          if (error.name === "NotAllowedError") playerMessage("Pulsa reproducir para comenzar.");
          else if (error.name !== "AbortError") fail("No se pudo iniciar el video. La fuente puede no estar disponible o usar un formato incompatible.");
        });
      }
      listenPlayer(video, "playing", () => { if (current()) { playerPaused = false; playerMessage("Reproduciendo"); } });
      listenPlayer(video, "pause", () => { if (current()) { playerPaused = true; savePlaybackPosition(true); playerMessage(video.ended ? "Reproducción finalizada" : "En pausa"); } });
      listenPlayer(video, "timeupdate", () => {
        if (current() && Number.isFinite(video.currentTime)) { playerPosition = video.currentTime; if (resumeApplied) savePlaybackPosition(); }
      });
      listenPlayer(video, "ended", () => { if (current()) savePlaybackPosition(true); });
      listenPlayer(video, "waiting", () => { if (current() && video.currentSrc) playerMessage("Cargando video…"); });
      listenPlayer(video, "loadedmetadata", () => { if (current()) { resumePosition(); applyPlayerAudio(video, hls, true); } });
      listenPlayer(video, "canplay", () => { if (current()) resumePosition(); });
      listenPlayer(video, "durationchange", () => { if (current()) resumePosition(); });
      listenPlayer(video, "error", () => {
        if (!current() || !video.error || !video.getAttribute("src")) return;
        fail(video.error.code === 2 ? "No se pudo cargar el video. Comprueba la conexión o vuelve a intentarlo."
          : "No se pudo decodificar el audio o video de esta fuente en Windows.");
      });
      if (video.audioTracks?.addEventListener) {
        ["addtrack", "change"].forEach((event) => listenPlayer(video.audioTracks, event, () => { if (current()) applyPlayerAudio(video, hls, video.readyState >= 1); }));
      }
      playerMessage("Cargando el video…");
      const useNative = await nativeReady;
      if (!current()) return;
      if (useNative) {
        await nativePlayer.open(source);
        if (current()) { playerControls?.attachEngine(null); beginPlayback(); }
      } else if (isHls && window.Hls?.isSupported()) {
        const engine = new Hls({ enableWorker: true, maxBufferLength: 30, backBufferLength: 30, xhrSetup(xhr, requestUrl) {
          if (new URL(requestUrl, url).origin === new URL(url).origin) headers.forEach(([name, value]) => xhr.setRequestHeader(name, String(value)));
        }});
        hls = engine;
        playerControls?.attachEngine(engine);
        engine.on(Hls.Events.ERROR, (_, data) => {
          if (data.fatal && current()) fail(data.type === Hls.ErrorTypes.NETWORK_ERROR
            ? "No se pudo acceder a la transmisión. Comprueba la conexión o vuelve a intentarlo." : "Esta transmisión no se pudo decodificar en Windows.");
        });
        engine.on(Hls.Events.MANIFEST_PARSED, () => {
          if (current()) { applyPlayerAudio(video, engine, true); beginPlayback(); }
        });
        [Hls.Events.AUDIO_TRACKS_UPDATED, Hls.Events.AUDIO_TRACK_SWITCHED].filter(Boolean).forEach((event) => {
          engine.on(event, () => { if (current()) applyPlayerAudio(video, engine, true); });
        });
        engine.loadSource(url); engine.attachMedia(video);
      } else {
        if (headers.length) throw new Error("Esta fuente de video todavía no es compatible con la aplicación de Windows.");
        if (isHls && !video.canPlayType("application/vnd.apple.mpegurl")) throw new Error("El reproductor de este equipo no admite esta transmisión.");
        video.src = url;
        beginPlayback();
      }
    } catch (error) { if (revision === playerRevision) { playerMessage(error.message, true); stopVideo(); } }
    finally { if (revision === playerRevision) { playerBusy = false; playerControls?.setBusy(false); renderPlayerOptions(); } }
  }
  function episodeItems(item) {
    if (item.kind !== "vod" || !Array.isArray(item.episodes)) return [];
    return item.episodes.filter((episode) => episode && episode.id !== undefined && episode.id !== null)
      .map((episode) => ({ ...item, ...episode, id: String(episode.id), kind: "vod", series_title: item.series_title || item.title, season_id: item.season_id || item.id, title: String(episode.title || `Episodio ${episode.number || ""}`) }));
  }
  playerControls = window.NebulaPlayerControls?.create({
    video: $("video"), dialog: $("player-dialog"), host: $("player-controls"), notify: toast,
    onEpisode: (episode) => showPlayer(episode),
    listSubtitles: (item) => call("playback.subtitles", { id: item.id, kind: item.kind }),
    loadSubtitle: (item, trackId) => call("playback.subtitle", { id: item.id, kind: item.kind, track_id: trackId }),
    saveReport: (item, category, description) => {
      const video = $("video"), finite = (value) => Number.isFinite(value) ? value : null;
      return call("playback.report", { id: item.id, kind: item.kind, category, description, playback: {
        current_time: finite(video.currentTime), duration: finite(video.duration), video_width: video.videoWidth,
        video_height: video.videoHeight, error_code: video.error?.code || null, ...playbackPreferences(),
      } });
    },
  }) || null;
  $("player-quality").addEventListener("change", () => {
    if (playerBusy || !activePlayback) return;
    const quality = playerOptions?.qualities.find((entry) => entry.value === $("player-quality").value);
    if (!quality) return;
    const preferences = { quality: quality.value };
    if (quality.audio_languages.some((language) => language.value === playerSelection?.audio_language)) preferences.audio_language = playerSelection.audio_language;
    showPlayer(activePlayback, preferences);
  });
  $("player-audio").addEventListener("change", () => {
    const value = $("player-audio").value;
    if (nativePlayer?.active && /^vlc:\d+$/.test(value)) {
      const id=Number(value.slice(4));
      nativePlayer.selectAudio(id).then(success => { if (success) {
        const track=nativePlayer.audioTracks.find(track=>track.id===id);
        const language=languageCode(track?.language);
        try { if(language) localStorage.setItem("nebula.audio.language",language); } catch {}
        playerNotices.audio = ""; renderPlayerOptions();
      } });
      return;
    }
    if (!playerBusy && activePlayback) showPlayer(activePlayback, { quality: $("player-quality").value, audio_language: $("player-audio").value });
  });
  $("video").addEventListener("nativetrackschange", () => { renderPlayerOptions(); playerControls?.refreshSubtitles(); });
  $("video").addEventListener("nativeaudiochange", renderPlayerOptions);
  $("player-retry").addEventListener("click", async () => {
    const item = activePlayback;
    if (sessionExpired && !await reconnectSession()) return;
    if (item && activePlayback === item) showPlayer(item);
  });
  $("player-close").addEventListener("click", () => $("player-dialog").close());
  $("player-cast").addEventListener("click", () => openCast(activePlayback, $("player-cast")));
  $("details-close").addEventListener("click", () => $("details-dialog").close());
  $("details-dialog").addEventListener("close", () => { detailRevision++; });
  $("player-dialog").addEventListener("close", () => { playerRevision++; stopVideo(); activePlayback = null; playerControls?.closed(); });
  [$("details-dialog"), $("player-dialog")].forEach((dialog) => dialog.addEventListener("click", (event) => {
    if (dialog === $("player-dialog") && playerControls?.locked) return;
    if (event.target === dialog) { const rect = dialog.getBoundingClientRect(); if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) dialog.close(); }
  }));
  document.querySelectorAll(".nav-item").forEach((element) => element.addEventListener("click", () => navigate(element.dataset.view)));
  $("search-form").addEventListener("submit", (event) => { event.preventDefault(); state.filters={}; state.view = "search"; state.category = null; state.query = $("search-input").value.trim(); loadView(); });
  $("filters-open").addEventListener("click",openFilters);
  $("filters-close").addEventListener("click",()=>$("filters-dialog").close());
  $("filters-reset").addEventListener("click",()=>{$("filters-form").querySelectorAll("select").forEach(select=>{select.value=select.name==="type"?select.options[0].value:"";});});
  $("filters-form").addEventListener("submit",event=>{
    event.preventDefault();if($("filters-apply").disabled)return;
    state.filters=Object.fromEntries(Array.from($("filters-form").querySelectorAll("select"),select=>[select.name,select.value]).filter(([,value])=>value));
    state.view="vod";state.kind="vod";state.section="home";state.category=null;state.query="";$("search-input").value="";
    $("filters-dialog").close();loadView();
  });
  $("filters-dialog").addEventListener("click",event=>{if(event.target===$("filters-dialog")){const r=event.target.getBoundingClientRect();if(event.clientX<r.left||event.clientX>r.right||event.clientY<r.top||event.clientY>r.bottom)event.target.close();}});
  $("load-more").addEventListener("click", () => loadView(true));
  $("refresh").addEventListener("click", refreshView);
  $("reconnect").addEventListener("click", reconnectSession);
  window.addEventListener("beforeunload", stopVideo);
  window.addEventListener("pagehide", () => savePlaybackPosition(true));
  async function start() {
    window.NebulaCast.init({call,toast});
    window.NebulaReleaseUI.init({call,toast});
    await Promise.allSettled([connectionStatus(), loadView(), fetchFavorites().then(updateFavoriteButtons)]);
    if (window.__XUPER_SMOKE__) call("app.smoke", { title: document.title, navigation: document.querySelectorAll(".nav-item").length, hls: window.Hls?.version, bridge: true, content: $("results").textContent.slice(0, 500) }).catch(() => {});
  }
  start();
})();
