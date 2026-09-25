param(
  [Parameter(Mandatory = $true)][string]$Executable,
  [Parameter(Mandatory = $true)][string]$Output
)

$ErrorActionPreference = 'Stop'
$report = [ordered]@{
  schema = 'w14079-windows-loader-diagnostic-v1'
  source_sha = $env:GITHUB_SHA
  argv = @($Executable, '--version')
  cwd = (Get-Location).Path
  environment = [ordered]@{ PATH = $env:PATH; CODEX_EXEC_PATH = $env:CODEX_EXEC_PATH }
  executable = $Executable
  imports = @()
  resolved_imports = @()
  access_probe = @()
  observer_token = [ordered]@{
    flags = @('DISABLE_MAX_PRIVILEGE', 'LUA_TOKEN')
    write_restricted = $false
    restricting_sids = @('current-user', 'Everyone')
    limitation = 'The production token also includes per-root capability, logon, and Everyone SIDs; this disposable observer cannot infer those per-root capability SIDs from the binary alone.'
  }
  launch = $null
}

if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw "Executable not found: $Executable" }
$dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
$imports = @()
if ($dumpbin) {
  $imports = @(& $dumpbin.Source /DEPENDENTS $Executable 2>$null |
    Where-Object { $_ -match '^\s{2,}([A-Za-z0-9_.-]+\.dll)\s*$' } |
    ForEach-Object { $matches[1] } | Sort-Object -Unique)
}
$report.imports = $imports
$search = @($env:PATH -split ';' | Where-Object { $_ })
$resolved = foreach ($name in $imports) {
  $candidate = $null
  foreach ($dir in $search) {
    $p = Join-Path $dir $name
    if (Test-Path -LiteralPath $p -PathType Leaf) { $candidate = (Resolve-Path -LiteralPath $p).Path; break }
  }
  [ordered]@{ name = $name; path = $candidate }
}
$report.resolved_imports = @($resolved)

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
public static class RestrictedProbe {
  const uint TOKEN_QUERY=8, TOKEN_DUPLICATE=2, DISABLE_MAX_PRIVILEGE=1, LUA_TOKEN=4;
  const uint GENERIC_READ=0x80000000, GENERIC_EXECUTE=0x20000000, FILE_SHARE_READ=1, FILE_SHARE_WRITE=2, FILE_SHARE_DELETE=4, OPEN_EXISTING=3, INVALID_HANDLE_VALUE=0xffffffff;
  const uint CREATE_UNICODE_ENVIRONMENT=0x400, CREATE_NO_WINDOW=0x8000000;
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr p, uint a, out IntPtr t);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool CreateRestrictedToken(IntPtr b,uint f,uint n,IntPtr d,uint p,IntPtr s,uint r,IntPtr e,out IntPtr t);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool ConvertStringSidToSid(string s,out IntPtr sid);
  [DllImport("advapi32.dll", SetLastError=true)] static extern IntPtr CreateFileW(string p,uint access,uint share,IntPtr sa,uint creation,uint flags,IntPtr template);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool ImpersonateLoggedOnUser(IntPtr t);
  [DllImport("advapi32.dll", SetLastError=true)] static extern bool RevertToSelf();
  [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool CreateProcessAsUserW(IntPtr t,string app, string cmd, IntPtr pa, IntPtr ta,bool inherit,uint flags,IntPtr env,string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
  [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h,uint ms);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr h,out uint code);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct STARTUPINFO { public uint cb; public string r1,r2,r3; public uint x,y,xs,ys,xcount,ycount,fill,flags; public ushort show,res; public IntPtr r2x,r3x,hIn,hOut,hErr; }
  [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess,hThread; public uint pid,tid; }
  [StructLayout(LayoutKind.Sequential)] struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }
  public static object Probe(string exe,string cwd,string userSid,string everyoneSid) {
    IntPtr baseToken, token; if(!OpenProcessToken(Process.GetCurrentProcess().Handle,TOKEN_QUERY|TOKEN_DUPLICATE,out baseToken)) throw new Win32Exception();
    IntPtr user, everyone; if(!ConvertStringSidToSid(userSid,out user) || !ConvertStringSidToSid(everyoneSid,out everyone)) throw new Win32Exception();
    var restrictions = new SID_AND_ATTRIBUTES[2]; restrictions[0].Sid=user; restrictions[1].Sid=everyone;
    var size = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)); var mem=Marshal.AllocHGlobal(size*restrictions.Length);
    for(int i=0;i<restrictions.Length;i++) Marshal.StructureToPtr(restrictions[i],IntPtr.Add(mem,size*i),false);
    try { if(!CreateRestrictedToken(baseToken,DISABLE_MAX_PRIVILEGE|LUA_TOKEN,0,IntPtr.Zero,0,IntPtr.Zero,(uint)restrictions.Length,mem,out token)) throw new Win32Exception(); }
    finally { CloseHandle(baseToken); }
    bool readExecute; string readError=null; try { if(!ImpersonateLoggedOnUser(token)) throw new Win32Exception(); var h=CreateFileW(exe,GENERIC_READ|GENERIC_EXECUTE,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,IntPtr.Zero,OPEN_EXISTING,0,IntPtr.Zero); if(h==IntPtr.Zero || h.ToInt64()==-1) throw new Win32Exception(); CloseHandle(h); readExecute=true; } catch(Exception e){readExecute=false;readError=e.ToString();} finally { RevertToSelf(); }
    var si=new STARTUPINFO(); si.cb=(uint)Marshal.SizeOf(si); PROCESS_INFORMATION pi; bool launched=CreateProcessAsUserW(token,null,"\""+exe+"\" --version",IntPtr.Zero,IntPtr.Zero,false,CREATE_UNICODE_ENVIRONMENT|CREATE_NO_WINDOW,IntPtr.Zero,cwd,ref si,out pi); uint code=0; string launchError=launched?null:new Win32Exception(Marshal.GetLastWin32Error()).ToString();
    if(launched){ WaitForSingleObject(pi.hProcess,120000); GetExitCodeProcess(pi.hProcess,out code); CloseHandle(pi.hThread); CloseHandle(pi.hProcess); }
    CloseHandle(token); Marshal.FreeHGlobal(mem); return new object[]{readExecute,readError,launched,launchError,code};
  }
}
'@
$userSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$probe = [RestrictedProbe]::Probe((Resolve-Path -LiteralPath $Executable).Path, (Get-Location).Path, $userSid, 'S-1-1-0')
$report.observer_token.restricting_sids = @($userSid, 'S-1-1-0')
$report.access_probe = [ordered]@{ executable_read_execute = $probe[0]; read_execute_error = $probe[1] }
$report.launch = [ordered]@{ create_process = $probe[2]; error = $probe[3]; exit_code = ('0x{0:X8}' -f [uint32]$probe[4]) }
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Output -Encoding utf8
Get-Content -LiteralPath $Output
