const fs=require('node:fs'),vm=require('node:vm'),path=require('node:path'),assert=require('node:assert/strict');
class Video extends EventTarget {
 constructor(){super();this.parentElement={prepend(){}};this.classList={add(){},remove(){}};}
 get currentTime(){return 0;} set currentTime(v){} get duration(){return NaN;} get volume(){return 1;} set volume(v){} get muted(){return true;}set muted(v){}
 play(){return Promise.resolve();}pause(){}load(){}getAttribute(){return null;}removeAttribute(){}getVideoPlaybackQuality(){return {};}
}
let tick,clock=0;
const calls=[],notices=[],events=[];
let status={active:true,generation:1,state:2,time:5,duration:0,seekable:false,width:0,height:0,frames:0,audio_tracks:[],subtitle_tracks:[]};
const window={chrome:{webview:{addEventListener(){}}},addEventListener(){}};
const context={window,Event,EventTarget,console,performance:{now:()=>clock},setInterval:fn=>(tick=fn,1),clearInterval(){},
 document:{createElement:()=>({setAttribute(){},getContext:()=>({})}),querySelector:()=>({classList:{add(){},remove(){}}})}};
vm.runInNewContext(fs.readFileSync(path.join(__dirname,'../../frontend/native-video.js'),'utf8'),context);
(async()=>{
 const video=new Video();for(const name of ['durationchange','seeked'])video.addEventListener(name,()=>events.push(name));
 const driver=window.NebulaNativeVideo.create(video,async(command,params)=>{calls.push({command,params});return command==='vlc.capabilities'?{available:true}:command==='vlc.open'?{generation:1}:command==='vlc.status'?{...status}:{};},notice=>notices.push(notice));
 assert(await driver.initialize());await driver.open({session_id:'synthetic',url:'http://127.0.0.1/fixture'});await tick();
 video.pause();await video.play();assert(!calls.some(c=>c.command==='vlc.pause'),'Playback intents must wait until the opening input can pause');assert(!driver.canSeek);
 video.currentTime=120;assert.equal(video.currentTime,5);assert(!calls.some(c=>c.command==='vlc.seek'));assert(notices.length);
 const durationEvents=events.length;status={...status,state:3,duration:500,seekable:true,width:1920,height:1080};await tick();assert.equal(video.duration,500);assert(events.length>durationEvents);assert(driver.canSeek);
 video.currentTime=120;assert(video.seeking);assert.equal(video.currentTime,5,'A request must not masquerade as actual decoder time');
 status.time=7;await tick();assert(video.seeking);assert.equal(video.currentTime,7);
 status.time=120;await tick();assert(!video.seeking);assert.equal(video.currentTime,120);
 video.currentTime=300;status.time=122;clock=11000;await tick();assert(!video.seeking);assert.equal(video.currentTime,122);assert(notices.some(n=>n.includes('No se pudo')));
 driver.stop();console.log('PASS: late duration enables seeking; positions follow the decoder; rejected/timed-out seeks do not fake success.');
})().catch(e=>{console.error(e);process.exitCode=1;});
