param([Parameter(Mandatory)][string]$Executable,[Parameter(Mandatory)][string]$Icon)
$ErrorActionPreference='Stop'
# Embed the application's own ICO in the newly built PE, preserving other resources.
if(-not ('NebulaResourceIcon' -as [type])){
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
public static class NebulaResourceIcon {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr BeginUpdateResource(string path,bool delete);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool UpdateResource(IntPtr handle,IntPtr type,IntPtr name,ushort language,byte[] data,uint length);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool EndUpdateResource(IntPtr handle,bool discard);
 public static void Apply(string path,string icon){
  byte[] bytes=File.ReadAllBytes(icon);if(bytes.Length<6||BitConverter.ToUInt16(bytes,2)!=1)throw new InvalidDataException("Invalid ICO");
  int count=BitConverter.ToUInt16(bytes,4);if(count<1||count>32||bytes.Length<6+count*16)throw new InvalidDataException("Invalid ICO entries");
  IntPtr handle=BeginUpdateResource(path,false);if(handle==IntPtr.Zero)throw new System.ComponentModel.Win32Exception();
  bool finished=false;
  try{
   using(var stream=new MemoryStream())using(var group=new BinaryWriter(stream)){
    group.Write((ushort)0);group.Write((ushort)1);group.Write((ushort)count);
    for(int i=0;i<count;i++){
     int entry=6+i*16;uint length=BitConverter.ToUInt32(bytes,entry+8),offset=BitConverter.ToUInt32(bytes,entry+12);
     if(length>int.MaxValue||offset>bytes.Length||length>bytes.Length-offset)throw new InvalidDataException("Invalid ICO bounds");
     byte[] frame=new byte[(int)length];Buffer.BlockCopy(bytes,(int)offset,frame,0,frame.Length);
     if(!UpdateResource(handle,(IntPtr)3,(IntPtr)(i+1),0,frame,length))throw new System.ComponentModel.Win32Exception();
     group.Write(bytes,entry,12);group.Write((ushort)(i+1));
    }
    byte[] directory=stream.ToArray();if(!UpdateResource(handle,(IntPtr)14,(IntPtr)1,0,directory,(uint)directory.Length))throw new System.ComponentModel.Win32Exception();
   }
   if(!EndUpdateResource(handle,false))throw new System.ComponentModel.Win32Exception();finished=true;
  }finally{if(!finished)EndUpdateResource(handle,true);}
 }
}
'@
}
[NebulaResourceIcon]::Apply((Resolve-Path -LiteralPath $Executable).Path,(Resolve-Path -LiteralPath $Icon).Path)
