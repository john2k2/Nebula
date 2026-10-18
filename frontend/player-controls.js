"use strict";

(() => {
  const clamp = (value, minimum, maximum) => Math.min(maximum, Math.max(minimum, value));
  const time = (seconds) => {
    if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
    const whole = Math.floor(seconds), hours = Math.floor(whole / 3600);
    return `${hours ? `${hours}:` : ""}${hours ? String(Math.floor(whole / 60) % 60).padStart(2, "0") : Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
  };
  const element = (tag, className, text) => {
    const result = document.createElement(tag);
    if (className) result.className = className;
    if (text !== undefined) result.textContent = String(text);
    return result;
  };
  function glyph(name) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS(svg.namespaceURI, "use");
    use.setAttribute("href", `#i-${name}`); svg.append(use);
    return svg;
  }
  function control(id, name, label, action) {
    const button = element("button", "transport-button");
    button.type = "button"; button.id = id; button.title = label; button.setAttribute("aria-label", label);
    button.append(glyph(name)); button.addEventListener("click", action);
    return button;
  }
  function labeledSelect(id, label, entries) {
    const field = element("label", "player-control-field"), select = element("select");
    select.id = id; field.htmlFor = id; field.append(element("span", "", label), select);
    entries.forEach(([value, text]) => { const option = element("option", "", text); option.value = value; select.append(option); });
    return { field, select };
  }
  function readPreferences() {
    try { return JSON.parse(localStorage.getItem("nebula.player.preferences") || "{}"); }
    catch { return {}; }
  }
  const subtitleName = (value) => typeof value === "string" ? value.trim().toLocaleLowerCase().slice(0, 160) : "";

  class PlayerControls {
    constructor(options) {
      this.options = options; this.video = options.video; this.dialog = options.dialog;
      this.surface = document.getElementById("player-surface");
      this.host = options.host; this.engine = null; this.engineCleanups = [];
      this.item = null; this.episodes = []; this.locked = false; this.busy = false; this.scrubbing = false;
      this.panel = null; this.contextRevision = 0; this.subtitleRevision = 0;
      this.remoteTracks = []; this.remoteStatus = "idle"; this.remoteTrack = null; this.remoteObjectUrl = null;
      this.subtitleChoice = "off"; this.subtitleIdentity = null; this.subtitleEntries = [];
      const preferences = readPreferences();
      const preferredSubtitle = preferences.subtitle;
      this.subtitlePreference = { enabled: preferredSubtitle?.enabled === true,
        language: subtitleName(preferredSubtitle?.language), label: subtitleName(preferredSubtitle?.label) };
      this.failedSubtitles = new Set();
      this.video.volume = Number.isFinite(preferences.volume) ? clamp(preferences.volume, 0, 1) : 1;
      this.video.muted = preferences.muted === true;
      this.subtitleSize = ["small", "medium", "large"].includes(preferences.subtitleSize) ? preferences.subtitleSize : "medium";
      this.subtitleStyle = ["white", "yellow", "dark"].includes(preferences.subtitleStyle) ? preferences.subtitleStyle : "white";
      this.build(); this.bind(); this.applySubtitleStyle();
      this.video.controls = false; this.video.tabIndex = 0; this.host.hidden = false;
      this.sync();
    }
    build() {
      const timeline = element("div", "player-timeline");
      this.timeline = timeline;
      this.elapsed = element("output", "player-time", "0:00"); this.elapsed.id = "player-elapsed";
      this.seek = element("input", "player-range player-seek"); this.seek.id = "player-seek";
      this.seek.type = "range"; this.seek.min = "0"; this.seek.max = "0"; this.seek.step = "0.1"; this.seek.value = "0";
      this.seek.setAttribute("aria-label", "Posición del vídeo");
      this.total = element("span", "player-time", "0:00"); this.total.id = "player-duration";
      timeline.append(this.elapsed, this.seek, this.total);
      this.main = element("div", "transport-main");
      const buttons = element("div", "transport-buttons");
      this.play = control("player-play", "play", "Reproducir (Espacio)", () => this.togglePlay());
      this.back = control("player-back", "rewind", "Retroceder 10 segundos (←)", () => this.seekBy(-10));
      this.forward = control("player-forward", "forward", "Avanzar 10 segundos (→)", () => this.seekBy(10));
      [this.back, this.forward].forEach((button) => button.append(element("span", "seek-seconds", "10")));
      this.previous = control("player-previous", "previous", "Episodio anterior (P)", () => this.changeEpisode(-1));
      this.next = control("player-next", "next", "Siguiente episodio (N)", () => this.changeEpisode(1));
      this.mute = control("player-mute", "volume", "Silenciar (M)", () => { this.video.muted = !this.video.muted; });
      this.volume = element("input", "player-range volume-range"); this.volume.id = "player-volume";
      this.volume.type = "range"; this.volume.min = "0"; this.volume.max = "100"; this.volume.step = "1";
      this.volume.setAttribute("aria-label", "Volumen");
      const volume = element("div", "volume-controls"); volume.append(this.mute, this.volume);
      buttons.append(this.play, this.back, this.forward, this.previous, this.next, volume);
      const settings = element("div", "transport-buttons transport-options");
      this.qualityButton = this.panelButton("player-quality-toggle", "quality", "Calidad del vídeo", "quality");
      const cast = document.getElementById("player-cast");
      cast.className = "transport-button";
      cast.setAttribute("aria-label", "Enviar al televisor con Chromecast");
      cast.replaceChildren(glyph("cast"));
      this.subtitlesButton = this.panelButton("player-subtitles-toggle", "subtitles", "Audio y subtítulos", "subtitles");
      this.episodesButton = this.panelButton("player-episodes-toggle", "episodes", "Episodios", "episodes");
      this.fullscreen = control("player-fullscreen", "fullscreen", "Pantalla completa (F)", () => this.toggleFullscreen());
      settings.append(this.qualityButton, cast, this.subtitlesButton, this.episodesButton, this.fullscreen);
      this.main.append(buttons, settings);
      const row = element("div", "transport-row");
      this.lock = control("player-lock", "lock", "Bloquear controles (B)", () => this.setLocked(!this.locked));
      this.lock.setAttribute("aria-pressed", "false"); row.append(this.main, this.lock);
      this.panelHost = element("div", "player-control-panels"); this.panelHost.hidden = true;
      this.panels = new Map();
      this.buildQuality(); this.buildSubtitles(); this.buildEpisodes(); this.buildSharing(); this.buildReport(); this.buildHelp();
      this.host.append(timeline, row, this.panelHost);
      this.loading = element("div", "player-loading"); this.loading.id = "player-loading";
      this.loading.setAttribute("role", "status"); this.loading.hidden = true;
      this.loading.append(element("span", "spinner"), element("span", "", "Cargando vídeo…"));
      this.video.parentElement.append(this.loading);
      this.subtitleOverlay = element("div", "player-subtitle-overlay"); this.subtitleOverlay.id = "player-subtitle-overlay";
      this.subtitleOverlay.hidden = true; this.video.parentElement.append(this.subtitleOverlay);
      this.captionCueChange = () => this.renderSubtitles(); this.captionRendered = [];
      this.subtitleResize = new ResizeObserver(() => this.updateSubtitleLayout());
      this.subtitleResize.observe(this.video); this.subtitleResize.observe(this.surface.querySelector(".player-bottom"));
    }
    panelButton(id, symbol, label, panel) {
      const button = control(id, symbol, label, () => this.togglePanel(panel));
      button.setAttribute("aria-expanded", "false"); button.setAttribute("aria-controls", `player-panel-${panel}`);
      return button;
    }
    addPanel(name, title, content) {
      const panel = element("section", "player-control-panel"); panel.id = `player-panel-${name}`; panel.hidden = true;
      const heading = element("div", "control-panel-heading"), label = element("h3", "", title); label.id = `player-panel-title-${name}`;
      panel.setAttribute("aria-labelledby", label.id);
      heading.append(label, control(`player-panel-close-${name}`, "close", "Cerrar opciones", () => this.togglePanel(null)));
      panel.append(heading, content); this.panelHost.append(panel); this.panels.set(name, panel);
    }
    buildQuality() {
      const content = element("div", "player-quality-controls player-settings");
      content.append(document.getElementById("player-quality-field"));
      this.qualityNote = element("p", "player-control-note");
      this.qualityNote.setAttribute("role", "status"); content.append(this.qualityNote);
      this.addPanel("quality", "Calidad del vídeo", content);
    }
    setQualityOptions(qualities, selected, notice) {
      const label = selected?.label || selected?.value;
      this.qualityButton.title = label ? `Calidad del vídeo: ${label}` : "Calidad del vídeo";
      this.qualityButton.setAttribute("aria-label", this.qualityButton.title);
      this.qualityNote.textContent = notice || (qualities.length === 1
        ? `Este título solo ofrece ${qualities[0].label} en la fuente actual.`
        : qualities.length ? "El cambio de calidad conserva la posición del vídeo." : "Esta fuente no ofrece otras calidades.");
    }
    buildSubtitles() {
      const content = element("div", "subtitle-controls");
      const language = labeledSelect("player-subtitle-language", "Idioma", [["off", "Desactivados"]]);
      this.subtitleSelect = language.select;
      const size = labeledSelect("player-subtitle-size", "Tamaño", [["medium", "Normal"], ["small", "Pequeño"], ["large", "Grande"]]);
      this.sizeSelect = size.select; this.sizeSelect.value = this.subtitleSize;
      const color = labeledSelect("player-subtitle-style", "Estilo", [["white", "Blanco"], ["yellow", "Amarillo"], ["dark", "Blanco sobre negro"]]);
      this.colorSelect = color.select; this.colorSelect.value = this.subtitleStyle;
      this.subtitleNote = element("p", "player-control-note", "Las pistas disponibles aparecerán aquí."); this.subtitleNote.setAttribute("role", "status");
      this.subtitleRetry = element("button", "button secondary", "Volver a consultar"); this.subtitleRetry.type = "button"; this.subtitleRetry.hidden = true;
      this.subtitleRetry.addEventListener("click", () => { this.remoteStatus = "idle"; this.ensureRemoteSubtitles(); });
      const audio = document.getElementById("player-settings");
      const captions = element("div", "subtitle-column"); captions.append(element("h4", "", "Subtítulos"), language.field, size.field, color.field);
      if (audio) content.append(audio);
      content.append(captions, this.subtitleNote, this.subtitleRetry);
      this.addPanel("subtitles", "Audio y subtítulos", content);
    }
    buildEpisodes() {
      this.episodeList = element("div", "player-episode-list");
      this.addPanel("episodes", "Episodios", this.episodeList);
    }
    buildSharing() {
      const content = element("div", "player-share-content");
      const label = element("label", "player-control-field", "Título y ficha"); label.htmlFor = "player-share-text";
      this.shareText = element("textarea", "player-textarea"); this.shareText.id = "player-share-text"; this.shareText.readOnly = true; this.shareText.rows = 4;
      label.append(this.shareText);
      const copy = element("button", "button secondary", "Copiar texto"); copy.type = "button";
      this.shareNote = element("p", "player-control-note", "Puedes copiar este texto y compartirlo donde prefieras."); this.shareNote.setAttribute("role", "status");
      copy.addEventListener("click", async () => {
        this.shareText.focus(); this.shareText.select();
        let copied = false;
        try { await navigator.clipboard.writeText(this.shareText.value); copied = true; }
        catch { try { copied = document.execCommand("copy"); } catch { /* The selected text remains available for Ctrl+C. */ } }
        this.shareNote.textContent = copied ? "Texto copiado." : "El texto está seleccionado. Pulsa Ctrl+C para copiarlo.";
      });
      content.append(label, copy, this.shareNote); this.addPanel("share", "Compartir título", content);
      this.shareButton = document.getElementById("player-share"); this.shareButton.hidden = false;
      this.shareButton.setAttribute("aria-controls", "player-panel-share"); this.shareButton.setAttribute("aria-expanded", "false");
      this.shareButton.addEventListener("click", async () => {
        if (!this.item) return;
        if (window.NebulaReleaseUI) { window.NebulaReleaseUI.share(this.item); return; }
        const text = [this.item.title, [this.item.year, this.item.category_title].filter(Boolean).join(" · "), this.item.description].filter(Boolean).join("\n\n");
        try { await navigator.clipboard.writeText(text); this.options.notify?.("Título y ficha copiados."); }
        catch { this.shareText.value = text; this.togglePanel("share"); this.shareText.focus(); this.shareText.select(); }
      });
    }
    buildReport() {
      const form = element("form", "player-report-form"); this.reportForm = form;
      const category = labeledSelect("player-report-category", "¿Qué ocurrió?", [
        ["video", "Problema con el vídeo"], ["audio", "Problema con el audio"], ["subtitles", "Problema con los subtítulos"],
        ["quality", "Calidad del vídeo"], ["cast", "Enviar al televisor"], ["other", "Otro problema"]]);
      const field = element("label", "player-control-field", "Descripción"); field.htmlFor = "player-report-description";
      const description = element("textarea", "player-textarea"); description.id = "player-report-description"; description.rows = 3;
      description.maxLength = 2000; description.required = true; description.placeholder = "Describe lo que ocurre al reproducir este título."; field.append(description);
      const explanation = element("p", "player-control-note", "El informe se guarda en esta PC. No se envía al proveedor.");
      const submit = element("button", "button", "Guardar informe en esta PC"); submit.type = "submit";
      this.reportStatus = element("p", "player-control-note"); this.reportStatus.setAttribute("role", "status");
      form.append(category.field, field, explanation, submit, this.reportStatus);
      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        if (!this.item || !description.value.trim() || submit.disabled || !this.options.saveReport) return;
        const context = this.contextRevision, item = this.item;
        submit.disabled = true; this.reportStatus.textContent = "Guardando el informe…";
        try {
          const result = await this.options.saveReport(item, category.select.value, description.value.trim());
          if (context !== this.contextRevision) return;
          if (result?.saved !== true || typeof result.path !== "string") throw new Error("No se pudo guardar el informe.");
          this.reportStatus.textContent = `Informe guardado en ${result.path}`;
        } catch (error) { if (context === this.contextRevision) this.reportStatus.textContent = error.message || "No se pudo guardar el informe. Vuelve a intentarlo."; }
        finally { submit.disabled = false; }
      });
      this.addPanel("report", "Informar un problema", form);
      this.reportButton = document.getElementById("player-report"); this.reportButton.hidden = !this.options.saveReport;
      this.reportButton.setAttribute("aria-controls", "player-panel-report"); this.reportButton.setAttribute("aria-expanded", "false");
      this.reportButton.addEventListener("click", () => this.togglePanel("report"));
    }
    buildHelp() {
      const content = element("div", "player-help-content"), shortcuts = element("dl", "player-shortcuts");
      this.helpVodHints = []; this.helpEpisodeHints = [];
      [
        ["Espacio / K", "Reproducir o pausar"], ["← / J · → / L", "Retroceder o avanzar 10 segundos", "vod"],
        ["↑ / ↓", "Subir o bajar el volumen"], ["M", "Silenciar o activar el sonido"],
        ["F", "Entrar o salir de pantalla completa"], ["B", "Bloquear o desbloquear controles"],
        ["P / N", "Episodio anterior o siguiente", "episode"], ["Esc", "Cerrar opciones o desbloquear controles"],
        ["H", "Abrir o cerrar esta ayuda"],
      ].forEach(([keys, description, kind]) => {
        const row = element("div", "player-shortcut"), term = element("dt"), detail = element("dd", "", description);
        term.append(element("kbd", "", keys)); row.append(term, detail); shortcuts.append(row);
        if (kind === "vod") this.helpVodHints.push(row);
        if (kind === "episode") this.helpEpisodeHints.push(row);
      });
      const gestures = element("div", "player-gesture-help"); gestures.append(element("h4", "", "Sobre el vídeo"));
      const horizontal = element("p", "", "Arrastra a los lados para buscar una posición."); this.helpVodHints.push(horizontal);
      gestures.append(element("p", "", "Doble clic para reproducir o pausar."), horizontal,
        element("p", "", "Arrastra arriba o abajo para ajustar el volumen."),
        element("p", "", "En pantalla completa, un clic muestra u oculta los controles. Mueve el puntero o usa el teclado para recuperarlos."));
      content.append(shortcuts, gestures, element("p", "player-control-note", "Los atajos funcionan cuando el foco está en el reproductor, fuera de un campo de texto o selector. El volumen solo afecta a este vídeo."));
      this.addPanel("help", "Atajos y gestos", content);
      this.helpButton = document.getElementById("player-help"); this.helpButton.hidden = false;
      this.helpButton.setAttribute("aria-controls", "player-panel-help"); this.helpButton.setAttribute("aria-expanded", "false");
      this.helpButton.addEventListener("click", () => this.togglePanel("help"));
    }
    bind() {
      ["loadedmetadata", "durationchange", "timeupdate", "progress", "seeked", "seeking", "play", "pause", "ended", "emptied", "canplay"].forEach((event) => this.video.addEventListener(event, () => this.sync()));
      this.video.addEventListener("ended", () => this.advanceOnEnd());
      this.video.addEventListener("loadedmetadata", () => { this.sourceChanging = false; this.refreshSubtitles(); });
      ["timeupdate", "seeked"].forEach((event) => this.video.addEventListener(event, () => this.renderSubtitles()));
      this.video.addEventListener("waiting", () => { this.waiting = true; this.sync(); });
      ["playing", "canplay", "seeked", "error", "emptied"].forEach((event) => this.video.addEventListener(event, () => {
        this.waiting = false; this.sync(); this.revealControls();
      }));
      this.video.addEventListener("pause", () => this.revealControls());
      this.video.addEventListener("volumechange", () => { this.syncVolume(); this.persist(); });
      this.video.textTracks?.addEventListener("addtrack", () => this.refreshSubtitles());
      this.video.textTracks?.addEventListener("removetrack", () => this.refreshSubtitles());
      this.video.textTracks?.addEventListener("change", () => this.refreshSubtitles());
      this.seek.addEventListener("input", () => {
        this.scrubbing = true; this.elapsed.textContent = time(Number(this.seek.value));
        this.seek.setAttribute("aria-valuetext", `${time(Number(this.seek.value))} de ${this.total.textContent}`); this.fillRange(this.seek);
      });
      this.seek.addEventListener("change", () => { this.seekTo(Number(this.seek.value)); this.scrubbing = false; this.sync(); });
      this.seek.addEventListener("pointercancel", () => { this.scrubbing = false; this.sync(); });
      this.volume.addEventListener("input", () => { this.video.volume = Number(this.volume.value) / 100; this.video.muted = false; this.syncVolume(); });
      this.subtitleSelect.addEventListener("change", () => this.selectSubtitle(this.subtitleSelect.value));
      this.sizeSelect.addEventListener("change", () => { this.subtitleSize = this.sizeSelect.value; this.applySubtitleStyle(); this.persist(); });
      this.colorSelect.addEventListener("change", () => { this.subtitleStyle = this.colorSelect.value; this.applySubtitleStyle(); this.persist(); });
      this.dialog.addEventListener("keydown", (event) => { this.pointerFocus = false; this.revealControls(); this.onKey(event); });
      this.dialog.addEventListener("cancel", (event) => {
        if (this.locked) { event.preventDefault(); this.setLocked(false); }
        else if (this.panel) { event.preventDefault(); this.togglePanel(null); }
      });
      this.video.title = "Doble clic para pausar o reproducir · F para pantalla completa";
      this.video.addEventListener("dblclick", () => { if (!this.locked && !this.ignoreDoubleClick) this.togglePlay(); });
      this.video.addEventListener("click", () => {
        if (this.ignoreDoubleClick || !this.dialog.open) return;
        if (this.surface.classList.contains("player-chrome-hidden")) this.revealControls();
        else this.hideControls();
      });
      this.surface.addEventListener("pointerdown", () => { this.pointerFocus = true; });
      this.surface.addEventListener("pointermove", (event) => {
        this.pointerOverControls = !!event.target.closest(".player-bottom, .player-heading"); this.revealControls();
      });
      this.surface.addEventListener("focusin", () => this.revealControls());
      this.bindGestures();
      document.addEventListener("fullscreenchange", () => {
        const active = document.fullscreenElement === this.surface;
        this.fullscreen.setAttribute("aria-label", active ? "Salir de pantalla completa (F)" : "Pantalla completa (F)");
        this.fullscreen.title = this.fullscreen.getAttribute("aria-label"); this.applySubtitleStyle(); this.revealControls();
      });
    }
    persist() {
      try { localStorage.setItem("nebula.player.preferences", JSON.stringify({ volume: this.video.volume, muted: this.video.muted,
        subtitleSize: this.subtitleSize, subtitleStyle: this.subtitleStyle,
        subtitle: this.subtitlePreference })); } catch { /* Settings remain usable in memory. */ }
    }
    setContext(item, episodes = []) {
      const changed = !this.item || this.item.id !== item.id || this.item.kind !== item.kind;
      this.item = item; this.episodes = episodes.filter((episode) => episode && typeof episode.id === "string");
      if (changed) {
        this.contextRevision++; this.subtitleRevision++;
        this.subtitleLoading = false; this.autoAdvancedItem = null;
        this.reportForm.reset(); this.reportStatus.textContent = ""; this.shareText.value = "";
        this.remoteTracks = []; this.remoteStatus = "idle"; this.subtitleChoice = "off"; this.subtitleIdentity = null;
        this.failedSubtitles.clear();
        this.removeRemoteTrack(); this.setLocked(false); this.togglePanel(null);
      }
      this.episodeList.replaceChildren();
      this.episodes.forEach((episode, index) => {
        const current = episode.id === item.id;
        const button = element("button", `player-episode${current ? " selected" : ""}`); button.type = "button";
        button.append(element("span", "episode-number", episode.number || index + 1), element("span", "episode-name", episode.title), glyph(current ? "pause" : "play"));
        if (current) button.setAttribute("aria-current", "true");
        button.disabled = current || this.busy;
        button.addEventListener("click", () => { if (!this.busy) this.options.onEpisode?.(episode); });
        this.episodeList.append(button);
      });
      this.refreshSubtitles(); this.sync();
    }
    setBusy(busy) {
      this.busy = !!busy; this.sync();
      if (!busy && this.subtitlePreference.enabled) this.ensureRemoteSubtitles();
    }
    sourceStopped() {
      this.engineCleanups.splice(0).forEach((cleanup) => cleanup()); this.engine = null;
      this.sourceChanging = true;
      this.setCaptionTrack(null);
      this.waiting = false; this.scrubbing = false; this.sync(); this.revealControls();
    }
    attachEngine(engine) {
      this.engineCleanups.splice(0).forEach((cleanup) => cleanup()); this.engine = engine;
      this.sourceChanging = false; this.engineSubtitlesReady = false;
      if (engine && window.Hls?.Events) {
        [Hls.Events.SUBTITLE_TRACKS_UPDATED, Hls.Events.SUBTITLE_TRACK_SWITCH, Hls.Events.MANIFEST_PARSED].filter(Boolean).forEach((event) => {
          const handler = () => {
            if (event !== Hls.Events.SUBTITLE_TRACK_SWITCH) this.engineSubtitlesReady = true;
            this.refreshSubtitles();
          }; engine.on(event, handler);
          this.engineCleanups.push(() => engine.off(event, handler));
        });
      }
      this.refreshSubtitles();
    }
    closed() {
      this.contextRevision++; this.subtitleRevision++; this.item = null; this.episodes = [];
      this.setLocked(false); this.togglePanel(null); this.removeRemoteTrack();
      this.remoteTracks = []; this.remoteStatus = "idle"; this.subtitleChoice = "off";
      this.subtitleLoading = false; this.subtitleIdentity = null; this.failedSubtitles.clear();
      this.setCaptionTrack(null);
      clearTimeout(this.chromeTimer); this.surface.classList.remove("player-chrome-hidden");
      if (document.fullscreenElement === this.surface) document.exitFullscreen().catch(() => {});
    }
    bounds() {
      if (Number.isFinite(this.video.duration) && this.video.duration > 0) return [0, this.video.duration];
      const seekable = this.video.seekable;
      return seekable?.length ? [seekable.start(0), seekable.end(seekable.length - 1)] : [0, 0];
    }
    fillRange(range) {
      const minimum = Number(range.min), maximum = Number(range.max), value = Number(range.value);
      range.style.setProperty("--range-fill", `${maximum > minimum ? clamp((value - minimum) / (maximum - minimum) * 100, 0, 100) : 0}%`);
    }
    sync() {
      const ready = this.video.readyState > 0 && !this.busy;
      const [minimum, maximum] = this.bounds();
      this.loading.hidden = !this.busy && !this.waiting;
      if (this.busy || this.waiting || this.video.paused) this.surface.classList.remove("player-chrome-hidden");
      this.play.disabled = !ready; this.back.disabled = this.forward.disabled = this.seek.disabled = !ready || maximum <= minimum;
      this.play.replaceChildren(glyph(this.video.paused ? "play" : "pause"));
      this.play.setAttribute("aria-label", this.video.paused ? "Reproducir (Espacio)" : "Pausar (Espacio)"); this.play.title = this.play.getAttribute("aria-label");
      if (!this.scrubbing) {
        this.seek.min = minimum; this.seek.max = maximum; this.seek.value = clamp(this.video.currentTime || 0, minimum, maximum);
        this.elapsed.textContent = time(this.video.currentTime); this.fillRange(this.seek);
        this.seek.setAttribute("aria-valuetext", `${time(this.video.currentTime)} de ${this.item?.kind === "live" ? "TV en vivo" : time(maximum)}`);
      }
      let buffered = this.video.currentTime || 0;
      for (let index = 0; index < this.video.buffered.length; index++) {
        if (this.video.buffered.start(index) <= buffered + 0.1 && this.video.buffered.end(index) >= buffered) buffered = this.video.buffered.end(index);
      }
      this.seek.style.setProperty("--buffer-fill", `${maximum > minimum ? clamp((buffered - minimum) / (maximum - minimum) * 100, 0, 100) : 0}%`);
      this.total.textContent = this.item?.kind === "live" && !Number.isFinite(this.video.duration) ? "EN VIVO" : time(maximum);
      const index = this.episodes.findIndex((episode) => episode.id === this.item?.id);
      const hasEpisodes = this.episodes.length > 1 && index >= 0;
      this.helpVodHints.forEach((hint) => { hint.hidden = this.item?.kind === "live"; });
      this.helpEpisodeHints.forEach((hint) => { hint.hidden = !hasEpisodes; });
      this.previous.hidden = this.next.hidden = this.episodesButton.hidden = !hasEpisodes;
      this.previous.disabled = this.busy || index <= 0; this.next.disabled = this.busy || index < 0 || index >= this.episodes.length - 1;
      this.episodeList.querySelectorAll("button").forEach((button, ordinal) => { button.disabled = this.busy || ordinal === index; });
      this.fullscreen.hidden = !document.fullscreenEnabled && document.fullscreenElement !== this.surface;
      this.syncVolume();
    }
    revealControls() {
      clearTimeout(this.chromeTimer); this.surface.classList.remove("player-chrome-hidden");
      this.updateSubtitleLayout();
      if (this.dialog.open && !this.video.paused && !this.busy && !this.waiting && !this.panel && !this.locked) {
        this.chromeTimer = setTimeout(() => this.hideControls(), 3000);
      }
    }
    hideControls() {
      if (!this.dialog.open || this.video.paused || this.busy || this.waiting || this.panel || this.locked || this.pointerOverControls) return;
      const focusedControl = document.activeElement?.closest(".player-bottom, .player-heading");
      if (focusedControl && !this.pointerFocus) return;
      if (focusedControl) this.video.focus({ preventScroll: true });
      clearTimeout(this.chromeTimer); this.surface.classList.add("player-chrome-hidden");
      this.updateSubtitleLayout();
    }
    syncVolume() {
      const muted = this.video.muted || this.video.volume === 0;
      this.mute.replaceChildren(glyph(muted ? "muted" : "volume"));
      this.mute.setAttribute("aria-label", muted ? "Activar sonido (M)" : "Silenciar (M)"); this.mute.title = this.mute.getAttribute("aria-label");
      this.mute.setAttribute("aria-pressed", String(muted));
      this.volume.value = this.video.muted ? 0 : Math.round(this.video.volume * 100);
      this.volume.setAttribute("aria-valuetext", `${this.volume.value}%`); this.fillRange(this.volume);
    }
    async togglePlay() {
      if (this.locked || this.busy || this.video.readyState < 1) return;
      if (this.video.paused) {
        try { await this.video.play(); }
        catch (error) { if (error.name !== "AbortError") this.options.notify?.("No se pudo iniciar la reproducción. Vuelve a intentarlo."); }
      } else this.video.pause();
    }
    seekTo(position) {
      if (this.locked || this.busy || !Number.isFinite(position) || this.video.readyState < 1) return;
      const [minimum, maximum] = this.bounds();
      if (maximum <= minimum) return;
      try { this.video.currentTime = clamp(position, minimum, maximum); }
      catch { this.options.notify?.("El vídeo todavía no permite ir a esa posición."); }
    }
    seekBy(seconds) {
      if (this.seek.disabled || this.locked) return;
      const before = this.video.currentTime; this.seekTo(before + seconds);
      this.feedback(`${seconds > 0 ? "+" : "−"}${Math.abs(seconds)} s`);
    }
    feedback(text) {
      const box = document.getElementById("player-gesture-feedback");
      clearTimeout(this.feedbackTimer); box.textContent = text; box.hidden = false;
      this.feedbackTimer = setTimeout(() => { box.hidden = true; }, 900);
    }
    async toggleFullscreen() {
      if (this.locked) return;
      try {
        if (document.fullscreenElement === this.surface) await document.exitFullscreen();
        else await this.surface.requestFullscreen();
      } catch { this.options.notify?.("No se pudo cambiar a pantalla completa en esta ventana."); }
    }
    setLocked(locked) {
      this.locked = locked; this.dialog.classList.toggle("controls-locked", locked);
      this.main.inert = this.timeline.inert = locked;
      const settings = document.getElementById("player-settings"); if (settings) settings.inert = locked;
      const actions = this.dialog.querySelector(".player-actions"); if (actions) actions.inert = locked;
      this.shareButton.disabled = this.reportButton.disabled = this.helpButton.disabled = locked;
      this.lock.setAttribute("aria-pressed", String(locked)); this.lock.replaceChildren(glyph(locked ? "unlock" : "lock"));
      this.lock.setAttribute("aria-label", locked ? "Desbloquear controles (B)" : "Bloquear controles (B)"); this.lock.title = this.lock.getAttribute("aria-label");
      document.getElementById("player-lock-notice").hidden = !locked;
      this.revealControls();
      if (locked) { this.togglePanel(null); this.lock.focus(); }
    }
    togglePanel(panel) {
      if (this.locked && panel) return;
      const next = this.panel === panel ? null : panel;
      this.panel = next;
      this.panels.forEach((element, name) => { element.hidden = name !== next; });
      [[this.qualityButton, "quality"], [this.subtitlesButton, "subtitles"], [this.episodesButton, "episodes"], [this.shareButton, "share"], [this.reportButton, "report"], [this.helpButton, "help"]].forEach(([button, name]) => button.setAttribute("aria-expanded", String(next === name)));
      this.panelHost.hidden = !next;
      this.revealControls();
      if (next === "subtitles") { this.ensureRemoteSubtitles(); this.refreshSubtitles(); }
      if (next) this.panels.get(next)?.querySelector("select, input, button:not(.transport-button)")?.focus();
    }
    changeEpisode(direction) {
      if (this.locked || this.busy) return;
      const index = this.episodes.findIndex((episode) => episode.id === this.item?.id), next = index + direction;
      if (index >= 0 && next >= 0 && next < this.episodes.length) this.options.onEpisode?.(this.episodes[next]);
    }
    advanceOnEnd() {
      if (!this.dialog.open || this.busy || !this.item || this.item.kind === "live" || this.autoAdvancedItem === this.item.id) return;
      const index = this.episodes.findIndex((episode) => episode.id === this.item.id);
      if (index < 0 || index + 1 >= this.episodes.length) return;
      this.autoAdvancedItem = this.item.id; this.options.onEpisode?.(this.episodes[index + 1]);
    }
    bindGestures() {
      this.video.addEventListener("pointerdown", (event) => {
        if (this.locked || this.busy || event.button !== 0 || this.video.readyState < 1) return;
        const rect = this.video.getBoundingClientRect();
        this.gesture = { id: event.pointerId, x: event.clientX, y: event.clientY, width: rect.width, height: rect.height,
          time: this.video.currentTime,
          volume: this.video.volume, muted: this.video.muted, mode: null, target: this.video.currentTime };
        this.video.setPointerCapture?.(event.pointerId);
      });
      this.video.addEventListener("pointermove", (event) => {
        const gesture = this.gesture; if (!gesture || event.pointerId !== gesture.id || this.locked) return;
        const dx = event.clientX - gesture.x, dy = event.clientY - gesture.y;
        if (!gesture.mode && Math.max(Math.abs(dx), Math.abs(dy)) < 12) return;
        gesture.mode ||= Math.abs(dx) > Math.abs(dy) ? "seek" : "volume";
        event.preventDefault();
        if (gesture.mode === "seek") {
          const [minimum, maximum] = this.bounds();
          gesture.target = clamp(gesture.time + dx / Math.max(1, gesture.width) * Math.min(maximum - minimum, 180), minimum, maximum);
          this.feedback(time(gesture.target));
        } else {
          this.video.volume = clamp(gesture.volume - dy / Math.max(1, gesture.height), 0, 1); this.video.muted = false;
          this.feedback(`Volumen ${Math.round(this.video.volume * 100)}%`);
        }
      });
      const finish = (event) => {
        const gesture = this.gesture; if (!gesture || gesture.id !== event.pointerId) return;
        this.gesture = null;
        if (gesture.mode) {
          this.ignoreDoubleClick = true; clearTimeout(this.doubleClickTimer);
          this.doubleClickTimer = setTimeout(() => { this.ignoreDoubleClick = false; }, 350);
          if (event.type === "pointerup" && gesture.mode === "seek") this.seekTo(gesture.target);
          this.persist();
        }
      };
      this.video.addEventListener("pointerup", finish); this.video.addEventListener("pointercancel", finish);
    }
    onKey(event) {
      if (!this.dialog.open || event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey) return;
      const key = event.key.toLowerCase();
      if (key === "escape") {
        if (this.locked) { event.preventDefault(); this.setLocked(false); }
        else if (this.panel) { event.preventDefault(); this.togglePanel(null); }
        return;
      }
      if (event.target.closest("input, select, textarea, [contenteditable=true]")) return;
      if (key === "b") { event.preventDefault(); this.setLocked(!this.locked); return; }
      if (this.locked) { if ([" ", "k", "j", "l", "f", "m", "n", "p", "arrowleft", "arrowright", "arrowup", "arrowdown"].includes(key)) event.preventDefault(); return; }
      const actions = { k: () => this.togglePlay(), arrowleft: () => this.seekBy(-10), j: () => this.seekBy(-10),
        arrowright: () => this.seekBy(10), l: () => this.seekBy(10), f: () => this.toggleFullscreen(),
        m: () => { this.video.muted = !this.video.muted; }, n: () => this.changeEpisode(1), p: () => this.changeEpisode(-1), h: () => this.togglePanel("help"),
        arrowup: () => { this.video.volume = clamp(this.video.volume + 0.05, 0, 1); this.video.muted = false; },
        arrowdown: () => { this.video.volume = clamp(this.video.volume - 0.05, 0, 1); this.video.muted = false; } };
      if (key === " " && !event.target.closest("button")) { event.preventDefault(); this.togglePlay(); }
      else if (actions[key]) { event.preventDefault(); actions[key](); }
    }
    async ensureRemoteSubtitles() {
      if (!this.item || this.item.kind === "live" || !this.options.listSubtitles || this.remoteStatus !== "idle") return;
      const revision = this.contextRevision, item = this.item;
      this.remoteStatus = "loading"; this.refreshSubtitles();
      try {
        const result = await this.options.listSubtitles(item);
        if (revision !== this.contextRevision || !this.dialog.open) return;
        this.remoteTracks = (Array.isArray(result?.tracks) ? result.tracks : []).filter((track) => track && typeof track.id === "string" && track.id)
          .map((track) => ({ id: track.id, label: String(track.label || track.language || "Subtítulos"), language: typeof track.language === "string" ? track.language : "" }));
        this.remoteStatus = "ready";
      } catch {
        if (revision !== this.contextRevision || !this.dialog.open) return;
        this.remoteStatus = "error";
      }
      this.refreshSubtitles();
    }
    refreshSubtitles() {
      // HLS may emit a synchronous switch event when a restored selection is applied.
      if (this.refreshingSubtitles) return;
      this.refreshingSubtitles = true;
      try {
      const entries = [];
      if (this.video.nativeEngine) {
        this.video.nativeEngine.subtitleTracks.forEach((track, index) => entries.push({ value: `vlc:${track.id}`, type: "vlc", index,
          id: String(track.id), language: track.language || "", label: track.label || track.language || `Pista ${index + 1}` }));
      } else if (this.engine?.subtitleTracks?.length) {
        this.engine.subtitleTracks.forEach((track, index) => entries.push({ value: `hls:${index}`, type: "hls", index,
          language: track.lang || "", label: track.name || track.lang || `Pista ${index + 1}` }));
      } else {
        Array.from(this.video.textTracks || []).forEach((track, index) => {
          if (track !== this.remoteTrack?.track && ["subtitles", "captions"].includes(track.kind)) entries.push({ value: `native:${index}`, type: "native", index,
            language: track.language || "", label: track.label || track.language || `Pista ${index + 1}` });
        });
      }
      this.remoteTracks.forEach((track, index) => entries.push({ value: `remote:${index}`, type: "remote", index,
        id: track.id, language: track.language, label: track.label }));
      this.subtitleEntries = entries;
      const preferred = this.preferredSubtitle(entries);
      if (!preferred) { this.subtitleChoice = "off"; this.disableSubtitles(); }
      else if (!this.subtitleLoading) this.applySubtitleEntry(preferred);
      this.subtitleSelect.replaceChildren(...[{ value: "off", label: "Desactivados" }, ...entries].map((entry) => {
        const option = element("option", "", entry.label); option.value = entry.value; return option;
      }));
      this.subtitleSelect.value = entries.some((entry) => entry.value === this.subtitleChoice) ? this.subtitleChoice : "off";
      this.subtitleSelect.disabled = entries.length === 0 || this.subtitleLoading;
      this.sizeSelect.disabled = this.colorSelect.disabled = entries.length === 0 || preferred?.type === "vlc";
      this.subtitleRetry.hidden = this.remoteStatus !== "error";
      this.subtitleNote.textContent = this.subtitleLoading ? "Cargando la pista seleccionada…"
        : this.remoteStatus === "loading" ? "Consultando subtítulos del título…"
        : this.remoteStatus === "error" ? "No se pudieron consultar los subtítulos del título. Las pistas del vídeo siguen disponibles."
        : this.subtitlePreference.enabled && !preferred && entries.length ? "El idioma de subtítulos guardado no está disponible en esta fuente. Puedes elegir otro."
        : preferred?.type === "vlc" ? "Esta pista integrada utiliza el estilo incluido en el vídeo."
        : entries.length ? "El tamaño y el color se aplican a los subtítulos del reproductor."
        : "Esta fuente no ofrece pistas de subtítulos disponibles.";
      } finally { this.refreshingSubtitles = false; }
    }
    preferredSubtitle(entries) {
      if (!this.subtitlePreference.enabled || this.sourceChanging) return null;
      const preferred = this.subtitleIdentity || this.subtitlePreference;
      if (preferred.type === "hls" && this.engine && !this.engineSubtitlesReady) return null;
      const language = subtitleName(preferred.language), label = subtitleName(preferred.label);
      const available = entries.filter((entry) => !this.failedSubtitles.has(this.subtitleKey(entry)));
      return (preferred.id && available.find((entry) => entry.type === preferred.type && entry.id === preferred.id))
        || available.find((entry) => entry.type === preferred.type && subtitleName(entry.language) === language && subtitleName(entry.label) === label)
        || (language && available.find((entry) => entry.type === preferred.type && subtitleName(entry.language) === language))
        || (language && available.find((entry) => subtitleName(entry.language) === language && subtitleName(entry.label) === label))
        || (language && available.find((entry) => subtitleName(entry.language) === language))
        || (label && available.find((entry) => subtitleName(entry.label) === label)) || null;
    }
    subtitleKey(entry) { return `${entry.type}:${entry.id || `${subtitleName(entry.language)}:${subtitleName(entry.label)}`}`; }
    applySubtitleEntry(entry) {
      this.subtitleChoice = entry.value;
      if (entry.type === "vlc" && this.video.nativeEngine) {
        if (this.remoteTrack) this.remoteTrack.track.mode = "disabled";
        this.setCaptionTrack(null);
        if (this.video.nativeEngine.selectedSubtitle !== Number(entry.id)) this.video.nativeEngine.selectSubtitle(Number(entry.id));
      } else if (entry.type === "hls") {
        // Hidden HLS tracks still load and synchronize cues, without native captions.
        this.engine.subtitleDisplay = false;
        if (this.engine.subtitleTrack !== entry.index) this.engine.subtitleTrack = entry.index;
        const selected = this.engine.subtitleTracks[entry.index], tracks = Array.from(this.video.textTracks || []);
        const track = tracks.includes(selected?.trackNode?.track) ? selected.trackNode.track
          : tracks.find((candidate) => candidate !== this.remoteTrack?.track && ["subtitles", "captions"].includes(candidate.kind)
            && subtitleName(candidate.language) === subtitleName(entry.language) && subtitleName(candidate.label) === subtitleName(entry.label));
        if (track) track.mode = "hidden";
        this.setCaptionTrack(track || null);
      } else if (entry.type === "native") {
        const track = this.video.textTracks[entry.index];
        this.disableSubtitles(track); if (track) track.mode = "hidden";
        this.setCaptionTrack(track || null);
      } else if (this.remoteTrackId === entry.id && this.remoteTrack) {
        this.disableSubtitles(this.remoteTrack.track); this.remoteTrack.track.mode = "hidden";
        this.setCaptionTrack(this.remoteTrack.track);
      } else if (!this.subtitleLoading) this.selectSubtitle(entry.value, false);
    }
    disableSubtitles(except) {
      if (this.video.nativeEngine && this.video.nativeEngine.selectedSubtitle !== -1) this.video.nativeEngine.selectSubtitle(-1);
      if (this.engine) { this.engine.subtitleDisplay = false; if (this.engine.subtitleTrack !== -1) this.engine.subtitleTrack = -1; }
      Array.from(this.video.textTracks || []).forEach((track) => { if (track !== except && ["subtitles", "captions"].includes(track.kind)) track.mode = "disabled"; });
      if (this.captionTrack !== except) this.setCaptionTrack(null);
    }
    async selectSubtitle(value, remember = true) {
      const revision = ++this.subtitleRevision, context = this.contextRevision;
      if (value === "off") {
        this.subtitlePreference = { enabled: false, language: "", label: "" }; this.subtitleIdentity = null;
        this.subtitleChoice = "off"; this.subtitleLoading = false; this.disableSubtitles();
        if (remember) this.persist(); this.refreshSubtitles(); return;
      }
      const entry = this.subtitleEntries.find((entry) => entry.value === value);
      if (!entry) { this.subtitleChoice = "off"; this.refreshSubtitles(); return; }
      this.subtitleIdentity = { type: entry.type, id: entry.id, language: entry.language, label: entry.label };
      this.subtitlePreference = { enabled: true, language: subtitleName(entry.language), label: subtitleName(entry.label) };
      this.failedSubtitles.delete(this.subtitleKey(entry)); this.subtitleChoice = value;
      if (remember) this.persist();
      if (entry.type !== "remote" || (this.remoteTrackId === entry.id && this.remoteTrack)) { this.refreshSubtitles(); return; }
      const track = this.remoteTracks[entry.index];
      if (!track || !this.options.loadSubtitle || !this.item) return;
      this.subtitleLoading = true; this.disableSubtitles(); this.refreshSubtitles();
      try {
        const result = await this.options.loadSubtitle(this.item, track.id);
        if (context !== this.contextRevision || revision !== this.subtitleRevision || !this.dialog.open) return;
        if (typeof result?.vtt !== "string" || new TextEncoder().encode(result.vtt).byteLength > 8 * 1024 * 1024 || !/^\uFEFF?WEBVTT(?:\s|$)/.test(result.vtt)) throw new Error("Invalid VTT");
        this.removeRemoteTrack();
        if (this.video.nativeEngine) {
          this.remoteTrack = this.video.nativeEngine.makeSubtitle(result.vtt, track.label, track.language);
          this.remoteTrackId = track.id; this.setCaptionTrack(this.remoteTrack.track); return;
        }
        const node = element("track"); node.kind = "subtitles"; node.label = track.label;
        if (/^[a-z]{2,3}(?:-[a-z0-9]{2,8})*$/i.test(track.language)) node.srclang = track.language;
        this.remoteObjectUrl = URL.createObjectURL(new Blob([result.vtt], { type: "text/vtt" })); node.src = this.remoteObjectUrl;
        this.remoteTrack = node; this.remoteTrackId = track.id; this.video.append(node); node.track.mode = "hidden";
        this.setCaptionTrack(node.track);
        node.addEventListener("error", () => { if (context === this.contextRevision && revision === this.subtitleRevision) this.options.notify?.("No se pudo abrir la pista de subtítulos."); }, { once: true });
      } catch {
        if (context !== this.contextRevision || revision !== this.subtitleRevision) return;
        this.failedSubtitles.add(this.subtitleKey(entry)); this.subtitleChoice = "off";
        this.options.notify?.("No se pudo cargar esa pista de subtítulos. Vuelve a seleccionarla para reintentar.");
      } finally {
        if (context === this.contextRevision && revision === this.subtitleRevision) { this.subtitleLoading = false; this.refreshSubtitles(); }
      }
    }
    removeRemoteTrack() {
      if (this.captionTrack === this.remoteTrack?.track) this.setCaptionTrack(null);
      this.remoteTrack?.remove(); this.remoteTrack = null; this.remoteTrackId = null;
      if (this.remoteObjectUrl) URL.revokeObjectURL(this.remoteObjectUrl);
      this.remoteObjectUrl = null;
    }
    applySubtitleStyle() {
      // DOM captions avoid Windows' forced user-agent ::cue styles. CSP stays intact.
      [this.video, this.subtitleOverlay].forEach((target) => {
        ["small", "medium", "large"].forEach((value) => target.classList.toggle(`subtitle-size-${value}`, value === this.subtitleSize));
        ["white", "yellow", "dark"].forEach((value) => target.classList.toggle(`subtitle-style-${value}`, value === this.subtitleStyle));
      });
      this.updateSubtitleLayout();
    }
    setCaptionTrack(track) {
      if (track !== this.captionTrack) {
        this.captionTrack?.removeEventListener("cuechange", this.captionCueChange);
        this.captionTrack = track; this.captionRendered = [];
        track?.addEventListener("cuechange", this.captionCueChange);
      }
      this.renderSubtitles();
    }
    renderSubtitles() {
      const track = this.captionTrack;
      const cues = this.dialog.open && !this.sourceChanging && this.subtitleChoice !== "off" && track?.mode === "hidden"
        ? Array.from(track.activeCues || []) : [];
      if (!cues.length) { this.subtitleOverlay.hidden = true; this.subtitleOverlay.replaceChildren(); this.captionRendered = []; return; }
      if (cues.length === this.captionRendered.length && cues.every((cue, index) => this.captionRendered[index].cue === cue && this.captionRendered[index].text === cue.text)) return;
      this.captionRendered = cues.map((cue) => ({ cue, text: cue.text }));
      this.subtitleOverlay.replaceChildren(...cues.map((cue) => {
        const line = element("div", "player-subtitle-line"), text = element("span", "player-subtitle-cue");
        if (typeof cue.getCueAsHTML === "function") {
          // Copy a small formatting allowlist; no classes, styles, URLs or handlers.
          const copy = (node, destination) => {
            if (node.nodeType === Node.TEXT_NODE) destination.append(document.createTextNode(node.textContent));
            else {
              const tag = node.nodeType === Node.ELEMENT_NODE ? node.tagName.toLowerCase() : "";
              const container = ["b", "i", "u", "ruby", "rt", "br"].includes(tag) ? element(tag) : destination;
              if (container !== destination) destination.append(container);
              Array.from(node.childNodes).forEach((child) => copy(child, container));
            }
          };
          copy(cue.getCueAsHTML(), text);
        } else text.textContent = String(cue.text || "");
        line.append(text); return line;
      }));
      this.subtitleOverlay.hidden = false; this.updateSubtitleLayout();
    }
    updateSubtitleLayout() {
      if (!this.subtitleOverlay) return;
      const overlayControls = this.dialog.open && !this.surface.classList.contains("player-chrome-hidden");
      this.subtitleOverlay.style.bottom = overlayControls
        ? `${Math.min(this.surface.querySelector(".player-bottom").getBoundingClientRect().height + 16, this.video.getBoundingClientRect().height * 0.65)}px` : "";
    }
  }
  Object.defineProperty(window, "NebulaPlayerControls", { value: Object.freeze({ create: (options) => new PlayerControls(options) }) });
})();
