<#
Stack-frame gate for the Windows desktop release executable.

Tauri runs IPC command dispatch on the executable's main thread, and in release
builds it moves each command future by value before tokio boxes it. An
oversized future therefore lands on the main-thread stack and the process dies
with 0xc00000fd inside __chkstk (buzz-desktop 0.5.21 with mesh-llm: ~1.07 MB
spawn frames against a 1 MiB stack).

The gate reads the PE header and every `mov eax, imm32; call __chkstk` prologue
in .text, names the owning function through the PDB next to the executable,
and fails when:
  * the main-thread stack reserve is below -MinReserve, or
  * a frame larger than -MaxFrame belongs to a function not on the allow-list
    (Tauri and tokio spawn paths are never allowed).

Usage: powershell -File scripts/check-windows-stack-frames.ps1 path\to\buzz-desktop.exe
Exit code 0 = PASS, 1 = FAIL, 2 = cannot check (refuses rather than passing).
#>
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [int]$MaxFrame = 256KB,
  [long]$MinReserve = 8MB,
  # Dispatch functions the scan must resolve (substrings of mangled names).
  [string[]]$RequireDispatch = @('respond_async_serialized', '5tokio4task5spawn')
)
$ErrorActionPreference = 'Stop'
trap { Write-Output "REFUSE: $_"; exit 2 }

# Third-party frames that are large by design and never run on the main
# thread. Each entry is a substring of the symbol name and carries a reason.
$Allowed = @(
  # 1 MiB read buffer while hashing a model file; runs on a blocking worker.
  'mesh_llm_host_runtime6models17artifact_transfer15file_sha256_hex',
  # Package inspection during model staging; runs on a mesh worker.
  'skippy_runtime7package21inspect_layer_package',
  # Audio decode for voice import; the only production caller is
  # buzz-voice import_path inside spawn_blocking (huddle/tts_voice_import.rs).
  'symphonia_core3dsp3fft3Fft3fft'
)
$NeverAllowed = @('5tauri', '5tokio4task5spawn', '5tokio7runtime4task')

Add-Type -TypeDefinition @"
using System; using System.Collections.Generic; using System.Runtime.InteropServices;
public static class StackGate {
  [DllImport("dbghelp.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool SymInitializeW(IntPtr h, string path, bool invade);
  [DllImport("dbghelp.dll")] static extern uint SymSetOptions(uint o);
  [DllImport("dbghelp.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern ulong SymLoadModuleExW(IntPtr h, IntPtr f, string img, string mod, ulong b, uint sz, IntPtr d, uint fl);
  [DllImport("dbghelp.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool SymFromAddrW(IntPtr h, ulong a, out ulong disp, IntPtr sym);
  [DllImport("dbghelp.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool SymFromNameW(IntPtr h, string name, IntPtr sym);
  static readonly IntPtr H = new IntPtr(0x5151);
  static ulong Base;
  public static bool Load(string exe) {
    SymSetOptions(0x2);
    if (!SymInitializeW(H, System.IO.Path.GetDirectoryName(exe), false)) return false;
    Base = SymLoadModuleExW(H, IntPtr.Zero, exe, null, 0x140000000, 0, IntPtr.Zero, 0);
    return Base != 0;
  }
  static IntPtr Buf() { int sz = 88 + 4000; IntPtr p = Marshal.AllocHGlobal(sz); for (int i = 0; i < sz; i++) Marshal.WriteByte(p, i, 0); Marshal.WriteInt32(p, 0, 88); Marshal.WriteInt32(p, 80, 2000); return p; }
  public static string NameAt(uint rva) {
    IntPtr p = Buf(); ulong d; string s = null;
    if (SymFromAddrW(H, Base + rva, out d, p)) s = Marshal.PtrToStringUni(new IntPtr(p.ToInt64() + 84));
    Marshal.FreeHGlobal(p); return s;
  }
  public static long RvaOf(string name) {
    IntPtr p = Buf(); long r = -1;
    if (SymFromNameW(H, name, p)) r = (long)((ulong)Marshal.ReadInt64(p, 56) - Base);
    Marshal.FreeHGlobal(p); return r;
  }
  // Returns (frameSize << 32 | callRva) for every "mov eax, imm32; call target".
  public static List<ulong> Frames(byte[] b, uint va, uint raw, uint rsz, uint target) {
    var outp = new List<ulong>();
    for (uint i = 5; i + 5 <= rsz; i++) {
      if (b[raw + i] != 0xE8 || b[raw + i - 5] != 0xB8) continue;
      int rel = BitConverter.ToInt32(b, (int)(raw + i + 1));
      if ((uint)(va + i + 5 + rel) != target) continue;
      uint size = BitConverter.ToUInt32(b, (int)(raw + i - 4));
      outp.Add(((ulong)size << 32) | (va + i));
    }
    return outp;
  }
  // x64 unwind data: for the RUNTIME_FUNCTION that contains rva, the stack
  // allocation its UNWIND_INFO records (UWOP_ALLOC_LARGE / UWOP_ALLOC_SMALL),
  // following chained entries to the primary. -1 when no entry covers rva.
  static uint[] SecVa, SecSize, SecRaw;
  static uint[] FnBegin, FnEnd, FnUnwind;
  static int Off(uint rva) {
    for (int i = 0; i < SecVa.Length; i++) if (rva >= SecVa[i] && rva < SecVa[i] + SecSize[i]) return (int)(SecRaw[i] + rva - SecVa[i]);
    return -1;
  }
  public static int LoadUnwind(byte[] b, uint[] va, uint[] size, uint[] raw, uint pdataRva, uint pdataSize) {
    SecVa = va; SecSize = size; SecRaw = raw;
    int n = (int)(pdataSize / 12), o = Off(pdataRva);
    FnBegin = new uint[n]; FnEnd = new uint[n]; FnUnwind = new uint[n];
    for (int i = 0; i < n; i++) {
      FnBegin[i] = BitConverter.ToUInt32(b, o + 12 * i);
      FnEnd[i] = BitConverter.ToUInt32(b, o + 12 * i + 4);
      FnUnwind[i] = BitConverter.ToUInt32(b, o + 12 * i + 8);
    }
    return n;
  }
  public static long UnwindAlloc(byte[] b, uint rva) {
    int lo = 0, hi = FnBegin.Length - 1, hit = -1;
    while (lo <= hi) { int mid = (lo + hi) / 2; if (FnBegin[mid] <= rva) { hit = mid; lo = mid + 1; } else hi = mid - 1; }
    if (hit < 0 || rva >= FnEnd[hit]) return -1;
    uint unwind = FnUnwind[hit];
    for (int depth = 0; depth < 32; depth++) {
      int u = Off(unwind & ~1u);
      if (u < 0) return -1;
      int flags = b[u] >> 3, count = b[u + 2];
      long alloc = 0;
      for (int i = 0; i < count; ) {
        int op = b[u + 4 + 2 * i + 1] & 0xF, info = b[u + 4 + 2 * i + 1] >> 4;
        switch (op) {
          case 1:
            if (info == 0) { alloc += BitConverter.ToUInt16(b, u + 4 + 2 * (i + 1)) * 8L; i += 2; }
            else { alloc += BitConverter.ToUInt32(b, u + 4 + 2 * (i + 1)); i += 3; }
            break;
          case 2: alloc += info * 8 + 8; i += 1; break;
          case 4: case 8: i += 2; break;
          case 5: case 9: i += 3; break;
          default: i += 1; break;
        }
      }
      if (alloc > 0 || (flags & 0x4) == 0) return alloc;
      // Chained: the parent RUNTIME_FUNCTION follows the (even-padded) codes.
      int chain = u + 4 + 2 * ((count + 1) & ~1);
      unwind = BitConverter.ToUInt32(b, chain + 8);
    }
    return -1;
  }
}
"@

$Exe = (Resolve-Path $Exe).Path
$pdb = Join-Path (Split-Path $Exe) (([IO.Path]::GetFileNameWithoutExtension($Exe) -replace '-', '_') + '.pdb')
if (-not (Test-Path $pdb)) { Write-Output "REFUSE: no PDB at $pdb; the gate cannot name frames"; exit 2 }
$b = [IO.File]::ReadAllBytes($Exe)
$pe = [BitConverter]::ToInt32($b, 0x3C)
$nsec = [BitConverter]::ToUInt16($b, $pe + 6)
$optsz = [BitConverter]::ToUInt16($b, $pe + 20)
$opt = $pe + 24
if ([BitConverter]::ToUInt16($b, $opt) -ne 0x20B) { Write-Output 'REFUSE: not a PE32+ executable'; exit 2 }
$reserve = [BitConverter]::ToUInt64($b, $opt + 72)
$commit = [BitConverter]::ToUInt64($b, $opt + 80)
$text = $null
$secVa = @(); $secSize = @(); $secRaw = @()
for ($i = 0; $i -lt $nsec; $i++) {
  $o = $opt + $optsz + 40 * $i
  $secVa += [BitConverter]::ToUInt32($b, $o + 12); $secSize += [BitConverter]::ToUInt32($b, $o + 8); $secRaw += [BitConverter]::ToUInt32($b, $o + 20)
  if ([Text.Encoding]::ASCII.GetString($b, $o, 8).TrimEnd([char]0) -eq '.text') {
    $text = @{ Va = [BitConverter]::ToUInt32($b, $o + 12); Raw = [BitConverter]::ToUInt32($b, $o + 20); Size = [BitConverter]::ToUInt32($b, $o + 16) }
  }
}
if (-not $text) { Write-Output 'REFUSE: no .text section'; exit 2 }
# Exception directory (data directory 3) = the .pdata RUNTIME_FUNCTION table.
$pdataRva = [BitConverter]::ToUInt32($b, $opt + 112 + 24)
$pdataSize = [BitConverter]::ToUInt32($b, $opt + 112 + 28)
if (-not $pdataRva) { Write-Output 'REFUSE: no exception directory; cannot cross-check frames'; exit 2 }
$fnCount = [StackGate]::LoadUnwind($b, [uint32[]]$secVa, [uint32[]]$secSize, [uint32[]]$secRaw, $pdataRva, $pdataSize)
if (-not [StackGate]::Load($Exe)) { Write-Output 'REFUSE: dbghelp could not load the module'; exit 2 }
$chk = [StackGate]::RvaOf('__chkstk')
if ($chk -lt 0) { Write-Output 'REFUSE: __chkstk not found in the PDB'; exit 2 }

$failures = @()
Write-Output "main-thread stack reserve $reserve bytes, commit $commit bytes"
if ($reserve -lt $MinReserve) { $failures += "stack reserve $reserve < required $MinReserve" }
$frames = @([StackGate]::Frames($b, $text.Va, $text.Raw, $text.Size, [uint32]$chk) | Sort-Object -Descending)
# Empty evidence is a broken scan, never a clean binary: a Rust/Tauri release
# exe has thousands of __chkstk prologues and hundreds of thousands of unwind
# entries. Refuse rather than report PASS on zero frames.
if ($fnCount -lt 1000) { Write-Output "REFUSE: only $fnCount unwind entries parsed; the .pdata scan is broken"; exit 2 }
if ($frames.Count -lt 100) { Write-Output "REFUSE: only $($frames.Count) __chkstk prologues found; the scan is broken"; exit 2 }
# Coverage, not just volume: the scan must reach the dispatch path that
# overflowed (Tauri's respond_async_serialized and the tokio spawn it calls).
# Unrelated functions can satisfy the counts above; these names cannot.
$requiredDispatch = [ordered]@{}
foreach ($r in $RequireDispatch) { $requiredDispatch[$r] = $false }
foreach ($f in $frames) {
  $n = [StackGate]::NameAt([uint32]($f % 4294967296))
  if ($n) { foreach ($k in @($requiredDispatch.Keys)) { if (-not $requiredDispatch[$k] -and $n.Contains($k)) { $requiredDispatch[$k] = $true } } }
  if ($requiredDispatch.Values -notcontains $false) { break }
}
$missingDispatch = @($requiredDispatch.Keys | Where-Object { -not $requiredDispatch[$_] })
if ($missingDispatch) { Write-Output "REFUSE: no __chkstk prologue resolves to $($missingDispatch -join ', '); the scan does not cover the dispatch path"; exit 2 }
Write-Output "dispatch coverage: prologues resolved for $($RequireDispatch -join ', ')"
Write-Output ("{0} __chkstk prologues, {1} unwind entries; largest prologue {2} bytes" -f $frames.Count, $fnCount, ($frames[0] -shr 32))
# Columns: verdict, prologue allocation (the __chkstk argument), unwind-data
# allocation for the same function, symbol. The two readings come from
# different PE structures and must agree for every reported frame.
foreach ($f in $frames) {
  $size = $f -shr 32
  if ($size -le $MaxFrame) { break }
  $rva = [uint32]($f % 4294967296)
  $unwind = [StackGate]::UnwindAlloc($b, $rva)
  $name = [StackGate]::NameAt($rva)
  if (-not $name) { $name = ('<unknown @ 0x{0:x}>' -f $rva) }
  $ok = ($Allowed | Where-Object { $name.Contains($_) }) -and -not ($NeverAllowed | Where-Object { $name.Contains($_) })
  Write-Output ('  {0} {1,9} {2,9}  {3}' -f ($(if ($ok) { 'allowed ' } else { 'OVERSIZE' })), $size, $unwind, $name)
  if (-not $ok) { $failures += "$size-byte frame in $name" }
  if ($unwind -ne $size) { $failures += "unwind data records $unwind bytes for $name, prologue allocates $size" }
}
if ($failures.Count) { Write-Output 'FAIL'; $failures | ForEach-Object { Write-Output "  $_" }; exit 1 }
Write-Output 'PASS'
exit 0
