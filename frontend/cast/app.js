(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  let context, controller, remote, selection, preparing = false, closed = false;
  let controlRevision = 0;
  let cancelAudioOperation = null, audioOperationSession = null;
  const message = (text, error = false) => { $('status').textContent = text; $('status').classList.toggle('error', error); };
  const session = () => context?.getCurrentSession();
  const audioNotice = text => { $('audio-notice').textContent = text; $('audio-notice').hidden = !text; };
  function languageTag(value) {
    if (typeof value !== 'string' || !value) return '';
    try { return new Intl.Locale(value.replace(/_/g, '-')).toString().toLowerCase(); }
    catch { return value.toLowerCase(); }
  }
  function languageLabel(value) {
    const language = languageTag(value);
    if (!language) return '';
    try { return new Intl.DisplayNames(['es'], { type: 'language' }).of(language); }
    catch { return value; }
  }
  function renderPreferences(preferences) {
    const values = [];
    if (preferences?.quality) values.push(`Calidad: ${preferences.quality}`);
    if (preferences?.audio_language) values.push(`Idioma solicitado: ${languageLabel(preferences.audio_language)}`);
    $('preferences').textContent = values.join(' · ');
    $('preferences').hidden = !values.length;
  }
  function cancelReceiverAudio() {
    const cancel = cancelAudioOperation;
    cancelAudioOperation = null; audioOperationSession = null;
    cancel?.();
  }
  function waitForReceiverMetadata(selectedSession, source, revision) {
    const current = () => revision === controlRevision && !closed && session() === selectedSession;
    const matchingMedia = () => {
      const media = selectedSession.getMediaSession?.();
      return media?.media?.contentId === source.url ? media : null;
    };
    return new Promise(resolve => {
      let finished = false, observed = null, timer;
      const eventType = cast.framework.SessionEventType?.MEDIA_SESSION;
      const canWatchSession = eventType && typeof selectedSession.addEventListener === 'function' && typeof selectedSession.removeEventListener === 'function';
      const cancel = () => finish(null);
      const stopObserving = () => { observed?.removeUpdateListener?.(check); observed = null; };
      function finish(media) {
        if (finished) return;
        finished = true; clearTimeout(timer); stopObserving();
        if (canWatchSession) selectedSession.removeEventListener(eventType, check);
        if (cancelAudioOperation === cancel) { cancelAudioOperation = null; audioOperationSession = null; }
        resolve(media);
      }
      function check() {
        if (!current()) { finish(null); return; }
        const media = selectedSession.getMediaSession?.();
        if (media !== observed) {
          stopObserving();
          if (typeof media?.addUpdateListener === 'function' && typeof media?.removeUpdateListener === 'function') {
            observed = media; observed.addUpdateListener(check);
          }
        }
        const matching = matchingMedia();
        const tracks = matching?.media?.tracks;
        if (Array.isArray(tracks) && tracks.some(track => track?.type === chrome.cast.media.TrackType?.AUDIO)
          && Array.isArray(matching.activeTrackIds)) finish(matching);
        else if (!canWatchSession && !observed) finish(matching);
      }
      cancelAudioOperation = cancel; audioOperationSession = selectedSession;
      timer = setTimeout(() => finish(current() ? matchingMedia() : null), 5000);
      if (canWatchSession) selectedSession.addEventListener(eventType, check);
      check();
    });
  }
  async function applyReceiverAudio(selectedSession, source, revision) {
    const wanted = languageTag(source.selection?.audio_language);
    if (!wanted) return;
    const mediaSession = await waitForReceiverMetadata(selectedSession, source, revision);
    const current = () => revision === controlRevision && !closed && session() === selectedSession
      && (!mediaSession || selectedSession.getMediaSession?.() === mediaSession);
    if (!current()) return;
    const report = text => { if (current()) audioNotice(text); };
    const tracks = Array.isArray(mediaSession?.media?.tracks) ? mediaSession.media.tracks : [];
    const audioType = chrome.cast.media.TrackType?.AUDIO;
    const audioTracks = audioType ? tracks.filter(track => track?.type === audioType && Number.isInteger(track.trackId)) : [];
    const chosen = audioTracks.find(track => languageTag(track.language) === wanted)
      || audioTracks.find(track => languageTag(track.language).split('-')[0] === wanted.split('-')[0]);
    const label = languageLabel(source.selection.audio_language);
    if (!mediaSession || !audioTracks.length) {
      report('El televisor no expone pistas de audio seleccionables. No se pudo aplicar el idioma solicitado; se conserva el audio que elija el receptor.');
      return;
    }
    if (!chosen) {
      report(`El televisor no ofrece una pista identificada como ${label}. No se cambió su audio.`);
      return;
    }
    const active = mediaSession.activeTrackIds;
    if (!Array.isArray(active)) {
      report('El televisor no informa qué pistas están activas. No se pudo cambiar el idioma conservando las demás pistas.');
      return;
    }
    if (active.includes(chosen.trackId)) { report(`Pista de audio activa en el televisor: ${label}.`); return; }
    if (typeof mediaSession.editTracksInfo !== 'function' || typeof chrome.cast.media.EditTracksInfoRequest !== 'function') {
      report('Este receptor no permite cambiar la pista de audio desde el control. El idioma solicitado no se pudo aplicar.');
      return;
    }
    // Cast IDs belong to the receiver, never to the APK's audioInfo index.
    // Retain active subtitles/video and replace only reported AUDIO track IDs.
    const audioIds = new Set(audioTracks.map(track => track.trackId));
    const activeTrackIds = [...new Set([...active.filter(id => !audioIds.has(id)), chosen.trackId])];
    if (!current()) return;
    const result = await new Promise(resolve => {
      let finished = false;
      const cancel = () => finish('canceled');
      const finish = result => {
        if (finished) return;
        finished = true; clearTimeout(timer);
        if (cancelAudioOperation === cancel) { cancelAudioOperation = null; audioOperationSession = null; }
        resolve(result);
      };
      const timer = setTimeout(() => finish('timeout'), 5000);
      cancelAudioOperation = cancel; audioOperationSession = selectedSession;
      try { mediaSession.editTracksInfo(new chrome.cast.media.EditTracksInfoRequest(activeTrackIds), () => finish('ok'), () => finish('error')); }
      catch { finish('error'); }
    });
    if (result === 'canceled') return;
    report(result === 'ok' ? `El televisor aceptó la pista de audio: ${label}.`
      : result === 'timeout' ? 'El televisor no confirmó el cambio de audio. No se puede asegurar que use el idioma solicitado.'
        : 'El televisor rechazó el cambio de pista. El idioma solicitado no se pudo aplicar; se conserva el audio anterior.');
  }
  const post = async action => {
    const response = await fetch(action, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}', credentials: 'omit', cache: 'no-store' });
    const value = await response.json();
    if (!response.ok) throw new Error(value.error || 'No se pudo completar la solicitud.');
    return value;
  };
  function update() {
    const connected = Boolean(session());
    $('device').textContent = connected ? (session().getCastDevice()?.friendlyName || 'Televisor conectado') : 'Sin conectar';
    $('send').disabled = closed || preparing || !connected || !selection;
    $('pause').disabled = closed || preparing || !remote?.isMediaLoaded;
    $('pause').textContent = remote?.isPaused ? 'Continuar' : 'Pausar';
    $('stop').disabled = closed || (!preparing && !connected);
  }
  window.__onGCastApiAvailable = available => {
    if (closed) return;
    if (!available) { message('Google Cast no está disponible. Abre este control con Google Chrome.', true); return; }
    context = cast.framework.CastContext.getInstance();
    context.setOptions({ receiverApplicationId: chrome.cast.media.DEFAULT_MEDIA_RECEIVER_APP_ID, autoJoinPolicy: chrome.cast.AutoJoinPolicy.ORIGIN_SCOPED });
    remote = new cast.framework.RemotePlayer();
    controller = new cast.framework.RemotePlayerController(remote);
    context.addEventListener(cast.framework.CastContextEventType.SESSION_STATE_CHANGED, () => {
      if (audioOperationSession && session() !== audioOperationSession) cancelReceiverAudio();
      update();
      if (!preparing) message(session() ? 'Televisor conectado. Ya puedes enviar el video.' : 'Elige un televisor de tu red.');
    });
    controller.addEventListener(cast.framework.RemotePlayerEventType.ANY_CHANGE, update);
    $('choose').disabled = false;
    message('Elige un televisor de tu red.'); update();
  };
  $('choose').addEventListener('click', async () => {
    try { await context.requestSession(); update(); }
    catch { message('No se conectó ningún televisor. Puedes volver a elegirlo.', true); }
  });
  $('send').addEventListener('click', async () => {
    if (!session() || preparing || closed) return;
    const selectedSession = session();
    const revision = ++controlRevision;
    cancelReceiverAudio();
    preparing = true; audioNotice(''); update(); message('Preparando el video para el televisor…');
    try {
      const source = await post('prepare');
      if (revision !== controlRevision || closed) return;
      if (session() !== selectedSession) { await post('stop'); throw new Error('El televisor cambió durante la preparación. Vuelve a elegir dónde reproducir.'); }
      renderPreferences(source.selection);
      const media = new chrome.cast.media.MediaInfo(source.url, source.mime_type);
      media.streamType = source.kind === 'live' ? chrome.cast.media.StreamType.LIVE : chrome.cast.media.StreamType.BUFFERED;
      media.metadata = new chrome.cast.media.GenericMediaMetadata();
      media.metadata.title = source.title;
      const request = new chrome.cast.media.LoadRequest(media);
      request.autoplay = true;
      if (source.kind !== 'live') request.currentTime = source.start_time || 0;
      try { await selectedSession.loadMedia(request); }
      catch { await post('stop').catch(() => {}); throw new Error('El televisor no pudo abrir el video. Comprueba que comparte la red de esta PC y que Windows permite la conexión de Nébula.'); }
      if (revision !== controlRevision || closed) return;
      if (session() !== selectedSession) { await post('stop'); throw new Error('El televisor cambió durante la carga. Vuelve a elegir dónde reproducir.'); }
      message('Reproduciendo en el televisor.');
      await applyReceiverAudio(selectedSession, source, revision);
    } catch (error) { if (revision === controlRevision && !closed) message(error.message || 'No se pudo preparar el video.', true); }
    finally { if (revision === controlRevision) { preparing = false; update(); } }
  });
  $('pause').addEventListener('click', () => { controller?.playOrPause(); });
  $('stop').addEventListener('click', async () => {
    controlRevision++; cancelReceiverAudio(); preparing = false; audioNotice('');
    Promise.resolve(session()?.endSession(true)).catch(() => {});
    try { await post('stop'); message('Envío detenido.'); } catch { message('No se pudo cerrar el envío. Mantén Nébula abierto y vuelve a intentarlo.', true); }
    update();
  });
  $('close').addEventListener('click', async () => {
    controlRevision++; cancelReceiverAudio(); preparing = false; closed = true; audioNotice('');
    $('choose').disabled = true; $('close').disabled = true; update();
    Promise.resolve(session()?.endSession(true)).catch(() => {});
    try { await post('close'); message('Control cerrado. Puedes cerrar esta pestaña.'); }
    catch { closed = false; $('choose').disabled = !context; $('close').disabled = false; update(); message('No se pudo cerrar el control. Inténtalo de nuevo.', true); }
  });
  fetch('state', { credentials: 'omit', cache: 'no-store' }).then(async response => {
    if (!response.ok) throw new Error();
    selection = await response.json(); $('title').textContent = selection.title; renderPreferences(selection.preferences); update();
  }).catch(() => { message('Este control ya no está disponible. Ábrelo otra vez desde Nébula.', true); });
  const sdk = document.createElement('script');
  sdk.src = 'https://www.gstatic.com/cv/js/sender/v1/cast_sender.js?loadCastFramework=1';
  sdk.onerror = () => message('No se pudo cargar Google Cast. Comprueba la conexión y vuelve a abrir este control.', true);
  document.head.append(sdk);
  setTimeout(() => { if (!context && !closed) message('Google Cast aún no está disponible. Comprueba que estás usando Google Chrome y que el televisor está encendido.', true); }, 15000);
})();
