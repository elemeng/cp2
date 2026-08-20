<#
test-ssh-mux.ps1 — diagnose whether Windows OpenSSH ControlMaster works on this
machine, and whether a stale cp2 control socket is interfering.

Usage:
    powershell -ExecutionPolicy Bypass -File test-ssh-mux.ps1
    powershell -ExecutionPolicy Bypass -File test-ssh-mux.ps1 -Target alice@host -Port 2222

Each ssh test authenticates normally (password or key); the first connection may
also ask to accept the host key. The script closes every master it opens and
prints a verdict at the end.

Windows PowerShell 5.1 compatible (no &&, no ternary).
#>
param(
    [string]$Target = "alice@host",
    [int]$Port = 22
)

$ErrorActionPreference = 'Continue'

if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
    Write-Error "ssh not found on PATH"
    exit 1
}
$ver = (cmd /c "ssh -V 2>&1" | Out-String).Trim()
Write-Host "ssh client: $ver"
Write-Host "target: $Target (port $Port)"
Write-Host ""

# Exact socket path the old cp2 used, a fresh name that rules out stale files,
# and the guide-style location under the user profile.
$cp2SockPath  = "$env:TEMP/cp2-ssh-%C"
$freshSock    = "$env:TEMP/cp2-mux-test-%C"
$userSockPath = "$env:USERPROFILE/.ssh/sockets/%r@%h-%p"
$userSockDir  = "$env:USERPROFILE\.ssh\sockets"

# ---------------------------------------------------------------- step 1
Write-Host "=== 1) stale control sockets in %TEMP% ==="
$stale = Get-ChildItem "$env:TEMP" -Filter "cp2-ssh-*" -ErrorAction SilentlyContinue
if ($stale) {
    $stale | Format-Table Name, Length, LastWriteTime -AutoSize
} else {
    Write-Host "(none)"
}
Write-Host ""

# ---------------------------------------------------------------- step 2
Write-Host "=== 2) exact old cp2 invocation (ControlPath=$cp2SockPath) ==="
Write-Host "    ssh -o ControlMaster=auto -o ControlPath=$cp2SockPath -o ControlPersist=60 ..."
ssh -p $Port -o ControlMaster=auto -o "ControlPath=$cp2SockPath" -o ControlPersist=60 $Target "echo MUX_CP2_OK"
$rc2 = $LASTEXITCODE
Write-Host "    exit code: $rc2"
if ($rc2 -eq 0) {
    ssh -p $Port -o "ControlPath=$cp2SockPath" -O exit $Target | Out-Null
}
Write-Host ""

# ---------------------------------------------------------------- step 3
Write-Host "=== 3) fresh socket name (ControlPath=$freshSock) ==="
Write-Host "    ssh -o ControlMaster=auto -o ControlPath=$freshSock -o ControlPersist=60 ..."
ssh -p $Port -o ControlMaster=auto -o "ControlPath=$freshSock" -o ControlPersist=60 $Target "echo MUX_FRESH_OK"
$rc3 = $LASTEXITCODE
Write-Host "    exit code: $rc3"
if ($rc3 -eq 0) {
    ssh -p $Port -o "ControlPath=$freshSock" -O exit $Target | Out-Null
}
Write-Host ""

# ---------------------------------------------------------------- step 4
Write-Host "=== 4) user-dir socket location (ControlPath=$userSockPath) ==="
New-Item -Force -ItemType Directory $userSockDir | Out-Null
Write-Host "    ssh -o ControlMaster=auto -o ControlPath=$userSockPath -o ControlPersist=60 ..."
ssh -p $Port -o ControlMaster=auto -o "ControlPath=$userSockPath" -o ControlPersist=60 $Target "echo MUX_USER_OK"
$rc4 = $LASTEXITCODE
Write-Host "    exit code: $rc4"
if ($rc4 -eq 0) {
    ssh -p $Port -o "ControlPath=$userSockPath" -O check $Target
    Write-Host "    (-O check exit code: $LASTEXITCODE)"
    ssh -p $Port -o "ControlPath=$userSockPath" -O exit $Target | Out-Null
}
Write-Host ""

# ---------------------------------------------------------------- verdict
Write-Host "=== verdict ==="
if ($rc2 -ne 0 -and $rc3 -ne 0) {
    Write-Host "MUX_BROKEN: even a fresh socket fails -> Win32-OpenSSH ControlMaster is unusable here."
    Write-Host "            Keep cp2's no-multiplexing behavior (already fixed); sessions each open their own connection."
}
elseif ($rc2 -ne 0 -and $rc3 -eq 0) {
    Write-Host "STALE_SOCKET: the old cp2 path fails but a fresh name works -> a stale cp2-ssh-* socket in %TEMP% is the culprit."
    Write-Host "              Fix: cp2 uses a unique socket name per run (or removes stale sockets before connecting)."
    Write-Host "              You can also delete the stale files now:"
    Write-Host "              Remove-Item `"$env:TEMP\cp2-ssh-*`""
}
elseif ($rc3 -eq 0 -and $rc4 -ne 0) {
    Write-Host "LOCATION: fresh %TEMP% socket works, ~/.ssh/sockets fails -> %TEMP% is the problem location."
    Write-Host "          Points at a cp2 ControlPath change (user-dir socket dir)."
}
else {
    Write-Host "MUX_OK: multiplexing works from the command line on this machine."
    Write-Host "        cp2's earlier failure is not reproducible via bare ssh; re-run cp2 with the fixed build to confirm."
}
