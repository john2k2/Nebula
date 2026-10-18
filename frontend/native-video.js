"use strict";
(() => {
  // Keep the existing media controls/history API while moving decoding and audio
  // selection to libVLC. The DOM video remains the accessible event surface.
  function create(video, call, notify) {
    const base = new Map(), methods = {};
    const state = { active: false, generation: 0, revision: 0, available: false, time: 0, duration: 0,
      paused: true, pauseIntent: null, volume: video.volume, muted: video.muted, width: 0, height: 0, ready: 0,
      ended: false, seeking: false, error: null, source: "", frames: 0, tracks: [], subtitles: [],
      selectedAudio: -1, selectedSubtitle: -1, engineState: null, seekable: false, diagnostics: {}, skipped: 0, lastSequence: 0 };
    const canvas = document.createElement("canvas"); canvas.id = "native-video-canvas"; canvas.hidden = true;
    canvas.setAttribute("aria-hidden", "true"); video.parentElement.prepend(canvas);
    const context = canvas.getContext("2d", { alpha: false });
    let pixels = null, polling = null, pollBusy = false, image = null, disposed = false;
    const emit = name => video.dispatchEvent(new Event(name));
    const commands = new Set(["pause", "seek", "volume", "audio", "subtitle"]);
    const send = (command, params = {}) => {
      if (!state.active || !commands.has(command)) return Promise.resolve(null);
      const generation = state.generation;
      return call(`vlc.${command}`, { generation, ...params }).catch(error => { if(state.active && state.generation === generation) notify?.(error.message); return null; });
    };
    function descriptor(name) {
      let prototype = video;
      while (prototype) { const found = Object.getOwnPropertyDescriptor(prototype, name); if (found) return found; prototype = Object.getPrototypeOf(prototype); }
      return null;
    }
    function property(name, get, set) {
      const original = descriptor(name); base.set(name, original);
      Object.defineProperty(video, name, { configurable: true,
        get() { return state.active ? get() : original?.get ? original.get.call(video) : original?.value; },
        set(value) { if (state.active) { set?.(value); } else if (original?.set) original.set.call(video, value); },
      });
    }
    for (const name of ["play", "pause", "load", "getAttribute", "removeAttribute", "getVideoPlaybackQuality"]) methods[name] = video[name].bind(video);
    property("currentTime", () => state.time, value => {
      if (!Number.isFinite(Number(value))) return;
      if (!state.seekable || ![3,4].includes(state.engineState)) { notify?.("Esta fuente todavía no permite cambiar de posición."); return; }
      state.seeking = true;
      state.seekTarget = Math.max(0, Math.min(Number(value), state.duration || Number(value))); state.seekBegan = performance.now();
      const revision = state.revision, target = state.seekTarget;
      emit("seeking"); send("seek", { time: target }).then(result => {
        if (!result && revision === state.revision && target === state.seekTarget) { state.seeking = false; emit("seeked"); }
      });
    });
    property("duration", () => state.duration || NaN);
    property("paused", () => state.paused); property("ended", () => state.ended);
    property("seeking", () => state.seeking); property("readyState", () => state.ready);
    property("videoWidth", () => state.width); property("videoHeight", () => state.height);
    property("currentSrc", () => state.source); property("error", () => state.error);
    const ranges = () => ({ length: state.duration > 0 ? 1 : 0, start: () => 0, end: () => state.duration });
    property("seekable", () => state.seekable ? ranges() : { length: 0 });
    // libVLC does not expose an accurate byte-buffer interval. Do not invent one.
    property("buffered", () => ({ length: 0 }));
    property("volume", () => state.volume, value => {
      state.volume = Math.max(0, Math.min(1, Number(value) || 0)); send("volume", { volume: state.volume, muted: state.muted }); emit("volumechange");
    });
    property("muted", () => state.muted, value => {
      state.muted = !!value; send("volume", { volume: state.volume, muted: state.muted }); emit("volumechange");
    });
    const audioEvents = new EventTarget();
    property("audioTracks", () => {
      const tracks = state.tracks.map(track => {
        const result = { id: String(track.id), label: track.label, language: track.language };
        Object.defineProperty(result, "enabled", { get: () => state.selectedAudio === track.id,
          set: enabled => { if (enabled && state.selectedAudio !== track.id) driver.selectAudio(track.id); } });
        return result;
      });
      tracks.addEventListener = audioEvents.addEventListener.bind(audioEvents);
      tracks.removeEventListener = audioEvents.removeEventListener.bind(audioEvents);
      return tracks;
    });
    video.play = () => {
      if (!state.active) return methods.play();
      const changed = state.paused; state.paused = false; state.pauseIntent = false; if (changed) emit("play");
      return [3,4].includes(state.engineState) ? send("pause", { paused: false }) : Promise.resolve();
    };
    video.pause = () => {
      if (!state.active) return methods.pause();
      const changed = !state.paused; state.paused = true; state.pauseIntent = true;
      if ([3,4].includes(state.engineState)) send("pause", { paused: true }); if (changed) emit("pause");
    };
    video.load = () => { if (state.active) driver.stop(); else methods.load(); };
    video.getAttribute = name => state.active && name === "src" ? state.source : methods.getAttribute(name);
    video.removeAttribute = name => { if (state.active && name === "src") return; methods.removeAttribute(name); };
    video.getVideoPlaybackQuality = () => state.active ? { totalVideoFrames: state.frames, droppedVideoFrames: state.skipped + (state.diagnostics.lost_frames || 0), creationTime: performance.now() } : methods.getVideoPlaybackQuality();
    async function poll() {
      if (pollBusy || !state.active) return;
      pollBusy = true; const revision = state.revision;
      try {
        const result = await call("vlc.status");
        if (!state.active || revision !== state.revision || result.generation !== state.generation || !result.active) return;
        const ready = state.ready > 0, previousPaused = state.paused, previousTime = state.time, previousDuration = state.duration, previousSeekable = state.seekable, previousEngineState = state.engineState;
        state.engineState = result.state;
        state.duration = result.duration; state.width = result.width; state.height = result.height;
        if (state.seeking && (Math.abs(result.time - state.seekTarget) < 1 || performance.now() - state.seekBegan > 10000)) {
          if (Math.abs(result.time - state.seekTarget) >= 1) notify?.("No se pudo cambiar de posición en esta fuente. Vuelve a intentarlo.");
          state.seeking = false; state.time = result.time; emit("seeked");
        } else state.time = result.time;
        state.seekable = result.seekable; state.selectedAudio = result.audio_track; state.selectedSubtitle = result.subtitle_track;
        state.diagnostics = { decoded_video_frames: result.decoded_video_frames, decoded_audio_blocks: result.decoded_audio_blocks, lost_frames: result.lost_frames };
        const signature = JSON.stringify(result.audio_tracks || []), oldSignature = JSON.stringify(state.tracks);
        state.tracks = result.audio_tracks || []; state.subtitles = result.subtitle_tracks || [];
        if (state.width > 0 || state.duration > 0) state.ready = state.width > 0 ? 4 : 1;
        const actualPaused = result.state !== 3;
        // VLC 3 stops an opening input if pause is requested before it can pause.
        // Apply the user's intent once the input reaches Playing/Paused instead.
        if (state.pauseIntent !== null && [3, 4].includes(result.state)) {
          if (actualPaused !== state.pauseIntent) send("pause", { paused: state.pauseIntent });
          else state.pauseIntent = null;
        }
        state.paused = state.pauseIntent ?? actualPaused; state.ended = result.state === 6;
        if (!ready && state.ready) { emit("loadedmetadata"); emit("durationchange"); emit("canplay"); }
        else if (state.duration !== previousDuration || state.seekable !== previousSeekable) emit("durationchange");
        if (ready && [3,4].includes(result.state) && ![3,4].includes(previousEngineState)) emit("canplay");
        if (signature !== oldSignature) { audioEvents.dispatchEvent(new Event("addtrack")); emit("nativetrackschange"); }
        if (result.state === 3 && !state.paused && (previousPaused || !ready || previousEngineState !== 3)) emit("playing");
        if (result.state === 4 && !previousPaused) emit("pause");
        if (result.state === 2) emit("waiting");
        if (state.time !== previousTime) emit("timeupdate");
        if (state.ended) { emit("ended"); clearInterval(polling); }
        if (result.state === 7) { state.error = { code: 3 }; emit("error"); clearInterval(polling); }
      } catch (error) { if (state.active && revision === state.revision) notify?.(error.message); }
      finally { pollBusy = false; }
    }
    window.chrome?.webview?.addEventListener("sharedbufferreceived", event => {
      if (event.additionalData?.kind !== "nebula-vlc-pixels") return;
      if (pixels) { try { window.chrome.webview.releaseBuffer(pixels); } catch {} }
      pixels = event.getBuffer(); image = null;
    });
    window.chrome?.webview?.addEventListener("message", event => {
      const data = event.data;
      if (data?.kind !== "nebula-vlc-frame") return;
      try {
        const strideWidth = (data.stride || data.width * 4) / 4;
        const length = strideWidth * data.height * 4;
        if (!state.active || data.generation !== state.generation || !pixels || length <= 0 || length > pixels.byteLength) return;
        if (!image || image.width !== strideWidth || image.height !== data.height || canvas.width !== data.width) {
          canvas.width = data.width; canvas.height = data.height;
          image = new ImageData(new Uint8ClampedArray(pixels, 0, length), strideWidth, data.height);
        }
        context.putImageData(image, 0, 0, 0, 0, data.width, data.height); state.frames++;
        if (state.lastSequence) state.skipped += Math.max(0, data.sequence - state.lastSequence - 1);
        state.lastSequence = data.sequence;
      } finally {
        window.ipc?.postMessage(JSON.stringify({ id: 0, command: "vlc.frameAck", params: { sequence: data.sequence } }));
      }
    });
    const driver = {
      get active() { return state.active; }, get available() { return state.available; },
      get canSeek() { return state.seekable && [3,4].includes(state.engineState); },
      get audioTracks() { return state.tracks; }, get selectedAudio() { return state.selectedAudio; },
      get subtitleTracks() { return state.subtitles; }, get selectedSubtitle() { return state.selectedSubtitle; },
      get diagnostics() { return state.diagnostics; },
      async initialize() {
        try {
          const result = await call("vlc.capabilities");
          state.available = result.available === true && !!window.chrome?.webview?.addEventListener;
          if (state.available) await call("vlc.buffer");
        } catch { state.available = false; }
        return state.available;
      },
      async open(source) {
        methods.pause(); methods.removeAttribute("src"); methods.load();
        const revision = ++state.revision;
        state.volume = base.get("volume").get.call(video); state.muted = base.get("muted").get.call(video);
        const result = await call("vlc.open", { session_id: source.session_id, volume: state.volume, muted: state.muted });
        if (revision !== state.revision || disposed) { call("vlc.stop", { generation: result.generation }).catch(() => {}); return false; }
        Object.assign(state, { active: true, generation: result.generation, time: 0, duration: 0, paused: false, pauseIntent: null,
          width: 0, height: 0, ready: 0, ended: false, engineState: null, selectedAudio: -1, selectedSubtitle: -1,
          seeking: false, seekTarget: 0, seekBegan: 0,
          error: null, source: source.url, frames: 0, tracks: [], subtitles: [], skipped: 0, lastSequence: 0 });
        video.classList.add("using-vlc"); canvas.hidden = false; image = null;
        document.querySelector('#player-surface').classList.add('native-vlc-active');
        clearInterval(polling); polling = setInterval(poll, 200); poll(); emit("play"); return true;
      },
      stop() {
        ++state.revision; clearInterval(polling); state.pauseIntent = null; state.seeking = false; state.seekTarget = 0; state.seekBegan = 0;
        if (!state.active) return;
        const generation = state.generation;
        state.active = false; state.ready = 0; state.source = "";
        base.get("volume").set.call(video, state.volume); base.get("muted").set.call(video, state.muted);
        video.classList.remove("using-vlc"); canvas.hidden = true;
        document.querySelector('#player-surface').classList.remove('native-vlc-active');
        call("vlc.stop", { generation }).catch(() => {}); emit("emptied");
      },
      async selectAudio(id) {
        const revision = state.revision;
        const result = await send("audio", { track: id });
        if (revision === state.revision && state.active && result?.audio_track === id) { state.selectedAudio = id; emit("nativeaudiochange"); return true; }
        return false;
      },
      selectSubtitle(id) { state.selectedSubtitle = id; return send("subtitle", { track: id }); },
      makeSubtitle(vtt, label, language) {
        const events = new EventTarget(), cues = [];
        const parseTime = value => value.split(":").reduce((seconds, part) => seconds * 60 + Number(part), 0);
        for (const block of vtt.replace(/\r/g, "").split(/\n\n+/)) {
          const lines = block.split("\n"), index = lines.findIndex(line => line.includes(" --> "));
          if (index < 0) continue;
          const [from, to] = lines[index].split(/\s+-->\s+/), start = parseTime(from), end = parseTime(to.split(/\s+/)[0]);
          if (Number.isFinite(start) && Number.isFinite(end) && end > start) cues.push(new VTTCue(start, end, lines.slice(index + 1).join("\n")));
        }
        const track = { label, language, kind: "subtitles", mode: "hidden", cues,
          get activeCues() { return cues.filter(cue => cue.startTime <= state.time && cue.endTime > state.time); },
          addEventListener: events.addEventListener.bind(events), removeEventListener: events.removeEventListener.bind(events) };
        return { track, remove() { track.mode = "disabled"; } };
      },
    };
    Object.defineProperty(video, "nativeEngine", { get: () => state.active ? driver : null });
    window.addEventListener("beforeunload", () => { driver.stop(); disposed = true; });
    return driver;
  }
  window.NebulaNativeVideo = Object.freeze({ create });
})();
