"use strict";
(() => {
  const node=(tag,text,cls)=>{const e=document.createElement(tag);if(text)e.textContent=text;if(cls)e.className=cls;return e;};
  const button=(text,fn,cls="button secondary")=>{const b=node("button",text,cls);b.type="button";b.onclick=fn;return b;};
  const clock=value=>{const n=Math.max(0,Math.floor(value||0));return `${Math.floor(n/3600)?Math.floor(n/3600)+":":""}${String(Math.floor(n/60)%60).padStart(2,"0")}:${String(n%60).padStart(2,"0")}`;};
  let api=null,dialog=null,timer=null,polling=false,scanning=false,context=null,onStarted=null,current={active:false},starting=false,startedNotified=false;
  let message,devices,list,controls,pause,stop,seek,elapsed,audio,notice,refresh,headerButton,statusLabel;
  let variants,qualitySelect,languageSelect,applyVariant,variantNote,options=null,sourceRevision=0,sourceLoading=false,selectedDevice=null;
  const sameItem=()=>!current.item?.id||!context?.id||current.item.id===context.id;
  function fill(select,rows,value){select.replaceChildren();for(const row of rows){const option=node("option",row.label||row.value);option.value=row.value;select.append(option);}select.value=rows.some(r=>r.value===value)?value:(rows[0]?.value||"");select.disabled=rows.length<2;}
  function languages(){const quality=options?.qualities?.find(row=>row.value===qualitySelect.value);fill(languageSelect,quality?.audio_languages||[],context?.audio_language);}
  function audioName(track){const aliases={spa:"es",eng:"en",kor:"ko",por:"pt",fra:"fr",fre:"fr",jpn:"ja"};try{const language=aliases[track.language]||track.language;return language?new Intl.DisplayNames(['es'],{type:'language'}).of(language):track.label;}catch{return track.label;}}
  async function loadVariants(){
    if(!context?.id){variants.hidden=true;return;}const revision=++sourceRevision;sourceLoading=true;variants.hidden=false;qualitySelect.disabled=true;languageSelect.disabled=true;applyVariant.disabled=true;variantNote.textContent="Consultando calidades e idiomas…";
    try{const data=await api.call("playback.options",{id:context.id,kind:context.kind,quality:context.quality,audio_language:context.audio_language});if(revision!==sourceRevision)return;
      options={qualities:(data.qualities||[]).filter(row=>typeof row.value==="string").map(row=>({...row,audio_languages:(row.audio_languages||[]).filter(a=>typeof a.value==="string")}))};
      fill(qualitySelect,options.qualities,context.quality||data.selected?.quality);context={...context,audio_language:context.audio_language||data.selected?.audio_language};if(qualitySelect.value)context.quality=qualitySelect.value;languages();
      variantNote.textContent=data.notice||"Elige calidad e idioma antes de enviar. Aplicar reinicia el envío desde su posición actual.";
    }catch(error){if(revision===sourceRevision)variantNote.textContent=error.message;}
    finally{if(revision===sourceRevision){sourceLoading=false;applyVariant.disabled=!selectedDevice||starting||current.preparing;}}
  }
  function tell(text,error=false){message.textContent=text;message.classList.toggle("error",error);}
  function render(status){
    current=status;if(status.active&&status.device_id)selectedDevice={id:status.device_id,name:status.device};
    if(!context&&status.active&&status.item?.id){context={...status.item,start_time:status.time||0};loadVariants();}
    headerButton.hidden=!(status.active||status.preparing);headerButton.textContent=status.active?`Televisor · ${status.device||"Conectado"}`:"Preparando televisor…";
    applyVariant.hidden=!status.active;applyVariant.disabled=sourceLoading||!selectedDevice||starting||status.preparing;
    controls.hidden=!(status.active||status.preparing);stop.disabled=!status.active&&!status.preparing;pause.disabled=!status.active||![3,4].includes(status.state);pause.textContent=status.state===4?"Continuar":"Pausar";
    statusLabel.textContent=status.active?`${status.title||"Nébula"} · ${status.device||"Televisor"}`:"Preparando el video…";
    if(document.activeElement!==seek){seek.max=String(Math.max(1,status.duration||0));seek.value=String(status.time||0);}seek.disabled=!status.active||!status.seekable;
    elapsed.textContent=`${clock(status.time)} / ${clock(status.duration)}`;
    const rows=status.audio_tracks||[],signature=JSON.stringify(rows);
    if(audio.dataset.tracks!==signature){audio.replaceChildren();for(const row of rows){const option=node("option",audioName(row)||`Audio ${row.id}`);option.value=String(row.id);audio.append(option);}audio.dataset.tracks=signature;}
    if(document.activeElement!==audio)audio.value=String(status.audio_track);audio.disabled=!status.active||rows.length<2;
    if(status.active&&status.state===7)tell("El envío encontró un problema. Comprueba el televisor y vuelve a intentarlo.",true);
    else if(status.active&&[1,2].includes(status.state))tell(`Conectando con ${status.device}…`);
    else if(status.active&&status.state===3){tell(`Enviando a ${status.device}.`);if(!startedNotified){startedNotified=true;onStarted?.();}}
    else if(status.active&&status.state===4)tell(`En pausa en ${status.device}.`);
    else if(status.active&&status.state===6)tell("Terminó la reproducción en el televisor.");
    if(status.active&&status.waiting_seconds>25&&status.time===0&&![4,6,7].includes(status.state))tell("El televisor todavía no confirma avance. Comprueba que ambos estén en la misma red y que la conexión de Nébula esté permitida en Windows.");
  }
  async function action(command,params={}){
    try{const result=await api.call(command,{generation:current.generation,...params});render(result);if(command==="cast.stop"){starting=false;tell("Se detuvo el envío al televisor.");await scan();}}
    catch(error){tell(error.message,true);}
  }
  async function scan(){
    if(scanning)return;scanning=true;refresh.disabled=true;
    try{const data=await api.call("cast.devices");const rows=data.devices||[];devices.hidden=Boolean(current.preparing||(current.active&&sameItem()));
      const signature=JSON.stringify(rows);if(list.dataset.devices!==signature){list.replaceChildren();for(const device of rows){const b=button(device.name,()=>send(device),"cast-device");b.setAttribute("aria-label",`Enviar a ${device.name}`);list.append(b);}list.dataset.devices=signature;}
      for(const b of list.querySelectorAll("button"))b.disabled=!context||starting||current.preparing;
      notice.textContent=rows.length?(context?"Elige el televisor donde quieres reproducir.":"Abre una película o un episodio y pulsa Enviar al televisor."):"Buscando televisores con Chromecast… Deben estar encendidos y conectados a la misma red que esta PC.";
      if(!rows.length&&!current.active&&!current.preparing)tell("Buscando televisores en tu red…");
      else if(rows.length&&!current.active&&!current.preparing&&!message.classList.contains("error"))tell("Elige un televisor de la lista.");
    }catch(error){notice.textContent=error.message;tell(error.message,true);}finally{scanning=false;refresh.disabled=false;}
  }
  async function poll(){
    if(polling)return;polling=true;
    try{const status=await api.call("cast.status");render(status);if(dialog?.open&&(!status.active||!sameItem())&&!status.preparing)await scan();}
    catch(error){if(dialog?.open)tell(error.message,true);}finally{polling=false;}
  }
  async function send(device){
    if(!context||starting)return;selectedDevice=device;starting=true;startedNotified=false;for(const b of list.querySelectorAll("button"))b.disabled=true;
    if(options){context={...context};if(qualitySelect.value)context.quality=qualitySelect.value;if(languageSelect.value)context.audio_language=languageSelect.value;}
    tell(`Preparando ${context.title} para ${device.name}…`);
    try{const result=await api.call("cast.start",{...context,device_id:device.id});render(result);devices.hidden=true;}
    catch(error){tell(error.message,true);}
    finally{starting=false;if(!current.active&&!current.preparing)await scan();}
  }
  function build(){
    dialog=node("dialog",null,"release-dialog cast-dialog");dialog.id="native-cast-dialog";dialog.setAttribute("aria-labelledby","native-cast-title");
    const heading=node("h2","Enviar al televisor");heading.id="native-cast-title";const intro=node("p","Chromecast integrado en Nébula. La PC debe permanecer encendida durante el envío.","release-note");
    message=node("p","Buscando televisores…","cast-message");message.setAttribute("role","status");
    variants=node("section",null,"cast-variants");variants.hidden=true;const qualityField=node("label","Calidad del envío"),languageField=node("label","Idioma de la fuente");qualitySelect=node("select");qualitySelect.setAttribute("aria-label","Calidad del envío");languageSelect=node("select");languageSelect.setAttribute("aria-label","Idioma de la fuente");qualitySelect.onchange=languages;qualityField.append(qualitySelect);languageField.append(languageSelect);variantNote=node("p",null,"release-note");applyVariant=button("Aplicar calidad e idioma",()=>{if(selectedDevice){context={...context,start_time:sameItem()?current.time||0:0};send(selectedDevice);}});applyVariant.hidden=true;variants.append(qualityField,languageField,variantNote,applyVariant);
    devices=node("section");notice=node("p",null,"release-note");list=node("div",null,"cast-devices");refresh=button("Buscar televisores",scan);devices.append(notice,list,refresh);
    controls=node("section",null,"cast-controls");controls.hidden=true;statusLabel=node("p");elapsed=node("p","00:00 / 00:00");seek=node("input");seek.type="range";seek.min="0";seek.max="1";seek.step="1";seek.setAttribute("aria-label","Posición del video en el televisor");seek.onchange=()=>action("cast.seek",{time:Number(seek.value)});
    pause=button("Pausar",()=>action("cast.pause",{paused:current.state!==4}));stop=button("Detener envío",()=>action("cast.stop"));
    const transport=node("div",null,"release-actions");transport.append(pause,stop);const field=node("label","Audio del televisor","cast-audio");audio=node("select");audio.setAttribute("aria-label","Audio del televisor");audio.onchange=()=>action("cast.audio",{track:Number(audio.value)});field.append(audio);
    controls.append(statusLabel,elapsed,seek,transport,field,node("p","Los idiomas disponibles dependen del título. Los cambios se aplican al envío del televisor.","release-note"));
    const footer=node("div",null,"release-actions"),close=button("Cerrar",()=>dialog.close());footer.append(close);dialog.append(heading,intro,message,variants,devices,controls,footer);
    document.body.append(dialog);
  }
  function open(item=null,started=null){
    if(!api)throw Error("Chromecast todavía se está preparando.");if(!dialog)build();const oldContext=context;context=item||(current.active?current.item||oldContext:null);onStarted=started;startedNotified=false;options=null;
    loadVariants();
    if(!dialog.open)dialog.showModal();tell("Buscando televisores en tu red…");poll();scan();
    if(!timer)timer=setInterval(()=>{if(dialog?.open||current.active||current.preparing)poll();},1500);
  }
  function init(options){api=options;headerButton=button("Televisor",()=>open(),"update-badge cast-indicator");headerButton.hidden=true;headerButton.setAttribute("aria-label","Control del televisor");document.querySelector(".topbar-actions").prepend(headerButton);}
  window.NebulaCast={init,open};
})();
