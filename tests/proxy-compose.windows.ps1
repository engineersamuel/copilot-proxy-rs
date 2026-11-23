Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http

function Assert($Condition, [string] $Message) {
    if (-not $Condition) { throw $Message }
}

function Invoke-Task([string] $Name, [string[]] $Options = @(), [switch] $Fail) {
    $previous = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $output = & mise --cd $script:fixture run "proxy:$Name" -- @Options 2>&1
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    $text = $output -join "`n"
    if ($Fail) {
        Assert ($code -ne 0) "proxy:$Name unexpectedly succeeded"
    } else {
        Assert ($code -eq 0) "proxy:$Name failed: $text"
    }
    return $text
}

function Get-State {
    $client = New-Object System.Net.Http.HttpClient
    try {
        [void] $client.DefaultRequestHeaders.Add('X-PC-Token-Key', $script:token)
        $body = $client.GetStringAsync($script:endpoint + '/processes').GetAwaiter().GetResult()
        return (ConvertFrom-Json $body).data[0]
    } finally {
        $client.Dispose()
    }
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$fixture = Join-Path ([IO.Path]::GetTempPath()) ("cpr windows test " + [Guid]::NewGuid().ToString('N'))
$sha = [Security.Cryptography.SHA256]::Create()
try {
    $hash = [BitConverter]::ToString($sha.ComputeHash(
        [Text.Encoding]::UTF8.GetBytes($fixture.ToLowerInvariant())
    )).Replace('-', '').Substring(0, 16).ToLowerInvariant()
} finally {
    $sha.Dispose()
}
$stateDir = Join-Path $env:LOCALAPPDATA "copilot-proxy-rs\process-compose\$hash"
$saved = @{}
foreach ($name in @('MISE_TRUSTED_CONFIG_PATHS', 'COPILOT_PROXY_RS_PC_PORT',
        'PC_SOCKET_PATH', 'PC_API_TOKEN', 'PC_ADDRESS', 'PC_PORT_NUM')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$listener = $null
$started = $false
try {
    [void] [IO.Directory]::CreateDirectory((Join-Path $fixture 'scripts'))
    Copy-Item (Join-Path $root 'mise.toml') $fixture
    Copy-Item (Join-Path $root 'scripts\proxy-compose.ps1') (Join-Path $fixture 'scripts')
    $config = @'
version: "0.5"
processes:
  proxy:
    command: powershell.exe -NoProfile -ExecutionPolicy Bypass -File worker.ps1
    ready_log_line: fixture-ready
    shutdown:
      timeout_seconds: 1
'@
    [IO.File]::WriteAllText((Join-Path $fixture 'process-compose.yaml'), $config)
    [IO.File]::WriteAllText((Join-Path $fixture 'worker.ps1'), @'
Write-Output 'fixture-ready'
Write-Output 'fixture-tail'
while ($true) { Start-Sleep -Seconds 1 }
'@)
    $env:MISE_TRUSTED_CONFIG_PATHS = $fixture
    $env:PC_SOCKET_PATH = 'surrounding-supervisor.sock'
    $env:PC_API_TOKEN = 'surrounding-supervisor-token-must-not-be-used'
    $env:PC_ADDRESS = '192.0.2.1'
    $env:PC_PORT_NUM = '1'
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $listener = $null
    $env:COPILOT_PROXY_RS_PC_PORT = "$port"
    $endpoint = "http://127.0.0.1:$port"

    $null = Invoke-Task 'down'
    $started = $true
    $null = Invoke-Task 'up'
    $tokenFile = Join-Path $stateDir 'api-token'
    $token = [IO.File]::ReadAllText($tokenFile)
    $state = Get-State
    Assert ($state.status -eq 'Running' -and $state.is_ready -eq 'Ready') 'fixture must be Running / Ready'
    $workerPid = $state.pid
    $null = Invoke-Task 'up'
    Assert ((Get-State).pid -eq $workerPid) 'repeated up must not restart the process'
    Assert ([IO.File]::ReadAllText($tokenFile) -eq $token) 'token must remain stable across task invocations'
    $acl = [IO.Directory]::GetAccessControl([IO.Path]::GetDirectoryName($tokenFile))
    Assert $acl.AreAccessRulesProtected 'token directory must not inherit access rules'
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    Assert ($rules.Count -eq 1 -and $rules[0].IdentityReference -eq
        [Security.Principal.WindowsIdentity]::GetCurrent().User) 'only the current user may access supervisor state'

    $client = New-Object System.Net.Http.HttpClient
    try {
        $response = $client.GetAsync("$endpoint/processes").GetAwaiter().GetResult()
        Assert ([int] $response.StatusCode -eq 401) 'control API must reject unauthenticated callers'
        $response.Dispose()
    } finally {
        $client.Dispose()
    }
    $status = Invoke-Task 'status'
    Assert ($status -match 'Running' -and $status -match 'Ready') 'status must reach the isolated supervisor'
    $logs = Invoke-Task 'logs' @('--tail', '1')
    Assert ($logs -match 'fixture-tail' -and $logs -notmatch 'fixture-ready') 'log options must be forwarded'
    $restart = Invoke-Task 'restart'
    Assert ($restart -match 'configuration unchanged') 'unchanged configuration must explicitly restart'
    Assert ((Get-State).pid -ne $workerPid) 'restart must replace the worker'
    $workerPid = (Get-State).pid
    [IO.File]::AppendAllText((Join-Path $fixture 'process-compose.yaml'), "`n    description: changed`n")
    $restart = Invoke-Task 'restart'
    Assert ($restart -match 'restarted with the new configuration') 'restart must reload changed configuration'
    Assert ((Get-State).pid -ne $workerPid) 'configuration reload must replace the worker'
    $workerPid = (Get-State).pid

    [IO.File]::WriteAllText($tokenFile, 'wrong-token-that-is-long-enough')
    try {
        $failure = Invoke-Task 'up' -Fail
        Assert ($failure -match 'HTTP 401') 'up must fail closed when another API owns the control port'
        Assert ($failure -notmatch [regex]::Escape($token)) 'errors must not reveal credentials'
    } finally {
        [IO.File]::WriteAllText($tokenFile, $token)
    }
    $null = Invoke-Task 'down'
    $started = $false
    Assert ($null -eq (Get-Process -Id $workerPid -ErrorAction SilentlyContinue)) 'down must stop the worker, not just its supervisor'
    $null = Invoke-Task 'down'
    $null = Invoke-Task 'status' -Fail
    foreach ($invalid in @('8080', '65536', 'not-a-port')) {
        $env:COPILOT_PROXY_RS_PC_PORT = $invalid
        $failure = Invoke-Task 'up' -Fail
        Assert ($failure -match 'COPILOT_PROXY_RS_PC_PORT must be') 'invalid control port must be rejected'
    }
    $env:COPILOT_PROXY_RS_PC_PORT = "$port"
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $port)
    $listener.Start()
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $failure = Invoke-Task 'down' -Fail
    Assert ($failure -match 'timed out' -and $timer.Elapsed.TotalSeconds -lt 20) 'hung endpoint must be bounded'
    Assert $listener.Server.IsBound 'failed control request must leave an unrelated listener intact'
    Write-Output 'Windows mise lifecycle, isolation, authentication, reload, log forwarding and failure checks passed'
} finally {
    if ($null -ne $listener) { $listener.Stop() }
    if ($started) { $null = Invoke-Task 'down' }
    foreach ($name in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
    }
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
    if (Test-Path -LiteralPath $stateDir) { Remove-Item -LiteralPath $stateDir -Recurse -Force }
}
