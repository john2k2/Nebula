"use strict";
(() => {
  const url="https://github.com/john2k2/Nebula/releases/latest";
  const el=(tag,text,className)=>{const n=document.createElement(tag);if(text)n.textContent=text;if(className)n.className=className;return n;};
  function dialog(title){
    const d=el("dialog",null,"release-dialog"),heading=el("h2",title),close=el("button","Cerrar","button secondary");
    heading.id="release-title-"+Math.random().toString(36).slice(2);d.setAttribute("aria-labelledby",heading.id);d.append(heading);close.type="button";
    close.onclick=()=>d.close();d.addEventListener("close",()=>d.remove());document.body.append(d);return {d,close};
  }
  function share(item){
    const {d,close}=dialog(item?"Compartir título":"Compartir Nébula");
    const text=[item?.title||"Nébula para Windows",item?.year,item?.score?`★ ${item.score} / 10`:null,item?.description?.slice(0,1500),item?"Encuéntralo en Nébula. Descarga la aplicación:":"Descarga la aplicación:",url].filter(Boolean).join("\n\n");
    const field=el("textarea",null,"player-textarea"),note=el("p","Comparte la ficha y el enlace de descarga de Nébula.","release-note"),actions=el("div",null,"release-actions"),copy=el("button","Copiar","button primary");
    field.value=text;field.readOnly=true;field.rows=7;field.setAttribute("aria-label","Texto para compartir");note.setAttribute("role","status");
    copy.onclick=async()=>{field.focus();field.select();let ok=false;try{await navigator.clipboard.writeText(text);ok=true;}catch{try{ok=document.execCommand("copy");}catch{}}note.textContent=ok?"Copiado. Ya puedes pegarlo donde quieras.":"Pulsa Ctrl+C para copiar el texto seleccionado.";};
    actions.append(copy);
    if(navigator.share){const native=el("button","Compartir…","button secondary");native.onclick=async()=>{try{await navigator.share({title:item?.title||"Nébula",text,url});}catch(error){if(error.name!=="AbortError")note.textContent="Usa Copiar para compartir el texto.";}};actions.append(native);}
    actions.append(close);d.append(note,field,actions);d.showModal();
  }
  function shareButton(item){const b=el("button",null,"icon-button detail-cast share-title");b.type="button";b.title="Compartir título";b.setAttribute("aria-label","Compartir título");b.innerHTML='<svg aria-hidden="true"><use href="#i-share"/></svg>';b.onclick=()=>share(item);return b;}
  function init({call,toast}){
    const footer=document.querySelector(".app-footer"),actions=el("div",null,"release-actions"),check=el("button","Buscar actualizaciones","footer-action"),shareApp=el("button","Compartir Nébula","footer-action"),badge=el("button","Nueva versión","update-badge");
    badge.hidden=true;badge.type="button";document.querySelector(".topbar-actions").prepend(badge);actions.append(check,shareApp);footer.append(actions);shareApp.onclick=()=>share();
    let checking=false,release=null;
    async function install(d,status,installButton,close){
      installButton.disabled=true;close.disabled=true;d.oncancel=event=>event.preventDefault();
      const poll=setInterval(()=>call("update.progress").then(progress=>{const labels={checking:"Consultando versión…",downloading:"Descargando actualización",verifying:"Verificando el paquete…",ready:"Reiniciando…"};status.textContent=labels[progress.phase]||"Preparando actualización…";if(progress.phase==="downloading"&&progress.total)status.textContent+=` · ${Math.floor(progress.downloaded/progress.total*100)} %`;}).catch(()=>{}),1000);
      try{status.textContent="Preparando actualización…";const result=await call("update.install");if(!result.restart)throw new Error("No se pudo preparar el reinicio.");window.dispatchEvent(new Event("pagehide"));call("update.exit").catch(()=>{});}
      catch(error){status.textContent=error.message;installButton.disabled=false;close.disabled=false;d.oncancel=null;}
      finally{clearInterval(poll);}
    }
    function show(){
      const {d,close}=dialog(release.available?`Nébula ${release.version}`:"Nébula está al día");
      const status=el("p",release.available?`Tu versión: ${release.current}. Se conservarán tus favoritos, el historial y la caché.`:`Versión instalada: ${release.current}.`),row=el("div",null,"release-actions");status.setAttribute("role","status");d.append(status);
      if(release.notes)d.append(el("p",release.notes,"release-notes"));
      if(release.available&&release.can_install){const installButton=el("button","Actualizar y reiniciar","button primary");installButton.onclick=()=>install(d,status,installButton,close);row.append(installButton);}
      if(release.available&&!release.can_install){const field=el("input");field.readOnly=true;field.value=url;field.setAttribute("aria-label","Página de descargas");d.append(el("p","Abre este enlace en tu navegador para descargar la versión:"),field);}
      row.append(close);d.append(row);d.showModal();
    }
    async function query(manual){if(checking)return;checking=true;check.disabled=true;try{release=await call("update.check");localStorage.setItem("nebula-update-check",String(Date.now()));badge.hidden=!release.available;if(manual)show();}catch(error){if(manual)toast(error.message);}finally{checking=false;check.disabled=false;}}
    check.onclick=()=>query(true);badge.onclick=()=>release&&show();
    if(!window.__XUPER_SMOKE__&&Date.now()-Number(localStorage.getItem("nebula-update-check")||0)>6*3600000)query(false);
  }
  window.NebulaReleaseUI={init,share,shareButton};
})();
