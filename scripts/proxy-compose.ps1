param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet('up', 'down', 'restart', 'status', 'logs', 'tui')]
    [string] $Command,
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)]
    [string[]] $LogOptions = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-NativeArguments([string[]] $Values) {
    ($Values | ForEach-Object {
        '"' + (($_ -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
    }) -join ' '
}

function Invoke-Control([string[]] $ControlArguments, [switch] $Interactive) {
    $values = $script:pcArguments + $ControlArguments
    if ($Interactive) {
        & $script:pc @values
        if ($LASTEXITCODE -ne 0) {
            throw "process-compose exited with code $LASTEXITCODE"
        }
        return
    }

    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $script:pc
    $info.Arguments = ConvertTo-NativeArguments $values
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    try {
        [void] $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) {
            $process.Kill()
            $process.WaitForExit()
            throw 'process-compose control request timed out'
        }
        $output = $stdout.GetAwaiter().GetResult()
        $errors = $stderr.GetAwaiter().GetResult()
        if ($output) { [Console]::Out.Write($output) }
        if ($errors) { [Console]::Error.Write($errors) }
        if ($process.ExitCode -ne 0) {
            throw "process-compose exited with code $($process.ExitCode)"
        }
    } finally {
        $process.Dispose()
    }
}

function Invoke-Api([string] $Path, [string] $Method = 'GET', [int] $Timeout = 10, [switch] $Probe) {
    $handler = New-Object System.Net.Http.HttpClientHandler
    $handler.UseProxy = $false
    $client = New-Object System.Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromSeconds($Timeout)
    [void] $client.DefaultRequestHeaders.Add('X-PC-Token-Key', $script:token)
    $request = New-Object System.Net.Http.HttpRequestMessage(
        (New-Object System.Net.Http.HttpMethod($Method)), "$script:endpoint$Path"
    )
    try {
        try {
            $response = $client.SendAsync($request).GetAwaiter().GetResult()
        } catch {
            $cause = $_.Exception
            while ($null -ne $cause) {
                if ($Probe -and $cause -is [System.Net.Sockets.SocketException] -and
                    $cause.SocketErrorCode -eq [System.Net.Sockets.SocketError]::ConnectionRefused) {
                    return $null
                }
                $cause = $cause.InnerException
            }
            throw "process-compose control request failed or timed out ($Method $Path); leaving the supervisor intact"
        }
        try {
            if ([int] $response.StatusCode -ne 200) {
                throw "process-compose control request failed ($Method $Path; HTTP $([int] $response.StatusCode))"
            }
            $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
            if ($body) {
                try { return ConvertFrom-Json -InputObject $body } catch {
                    throw "process-compose returned invalid JSON ($Method $Path)"
                }
            }
        } finally {
            $response.Dispose()
        }
    } finally {
        $request.Dispose()
        $client.Dispose()
    }
}

function Wait-Ready($Supervisor = $null) {
    $deadline = [DateTime]::UtcNow.AddSeconds(600)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -ne $Supervisor -and $Supervisor.HasExited) {
            throw "process-compose exited during startup; inspect $script:stateDir"
        }
        $state = Invoke-Api '/processes' -Probe
        if ($null -ne $state) {
            $proxy = @($state.data | Where-Object { $_.name -eq 'proxy' })
            if ($proxy.Count -eq 1 -and $proxy[0].status -eq 'Running' -and $proxy[0].is_ready -eq 'Ready') {
                Write-Output 'proxy is Ready'
                return
            }
        }
        Start-Sleep -Seconds 2
    }
    throw 'proxy did not become Ready within 600 seconds; inspect proxy:logs or run proxy:down'
}

try {
    Add-Type -AssemblyName System.Net.Http
    $repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    Set-Location $repoRoot
    $pc = (Get-Command process-compose.exe -CommandType Application -ErrorAction Stop).Source
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash(
            [Text.Encoding]::UTF8.GetBytes($repoRoot.ToLowerInvariant())
        )).Replace('-', '').Substring(0, 16).ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
    $stateDir = Join-Path $env:LOCALAPPDATA "copilot-proxy-rs\process-compose\$hash"
    [void] [IO.Directory]::CreateDirectory($stateDir)
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
        $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'
    )))
    [IO.Directory]::SetAccessControl($stateDir, $acl)

    $port = 49152 + ([Convert]::ToUInt32($hash.Substring(0, 4), 16) % 16384)
    if ($env:COPILOT_PROXY_RS_PC_PORT) {
        $port = 0
        if (-not [int]::TryParse($env:COPILOT_PROXY_RS_PC_PORT, [ref] $port) -or
            $port -lt 1024 -or $port -gt 65535 -or $port -eq 8080) {
            throw 'COPILOT_PROXY_RS_PC_PORT must be between 1024 and 65535, excluding proxy port 8080'
        }
    }
    if ($env:COPILOT_PROXY_RS_PORT -and $port -eq [int] $env:COPILOT_PROXY_RS_PORT) {
        throw 'The process-compose control port must differ from COPILOT_PROXY_RS_PORT'
    }
    $endpoint = "http://127.0.0.1:$port"
    $tokenFile = Join-Path $stateDir 'api-token'
    if (-not (Test-Path -LiteralPath $tokenFile)) {
        $bytes = New-Object byte[] 32
        $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
        try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
        # CreateNew prevents concurrent invocations from replacing an active supervisor's token.
        $file = [IO.File]::Open($tokenFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
        try {
            $bytes = [Text.Encoding]::ASCII.GetBytes([Convert]::ToBase64String($bytes))
            $file.Write($bytes, 0, $bytes.Length)
        } finally {
            $file.Dispose()
        }
    }
    $token = [IO.File]::ReadAllText($tokenFile).Trim()
    if ($token.Length -lt 20) { throw "Invalid process-compose API token file: $tokenFile" }

    # Do not inherit a surrounding supervisor's endpoint, authentication or log files.
    foreach ($name in @('PC_SOCKET_PATH', 'PC_API_TOKEN', 'PC_NO_SERVER', 'PC_READ_ONLY')) {
        [Environment]::SetEnvironmentVariable($name, $null, 'Process')
    }
    $env:PC_API_TOKEN_PATH = $tokenFile
    $env:PC_LOG_FILE = Join-Path $stateDir 'supervisor.log'
    $env:PC_CLIENT_LOG_FILE = Join-Path $stateDir 'client.log'
    $pcArguments = @('--address', '127.0.0.1', '--port', "$port", '--token-file', $tokenFile)

    switch ($Command) {
        'up' {
            $supervisor = $null
            if ($null -eq (Invoke-Api '/processes' -Probe)) {
                $startArguments = $pcArguments + @('-f', 'process-compose.yaml', '--tui=false')
                # Shell execution avoids inheriting mise's pipes and keeping its task alive.
                $info = New-Object System.Diagnostics.ProcessStartInfo
                $info.FileName = $pc
                $info.Arguments = ConvertTo-NativeArguments $startArguments
                $info.WorkingDirectory = $repoRoot
                $info.UseShellExecute = $true
                $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
                $supervisor = [Diagnostics.Process]::Start($info)
            }
            try { Wait-Ready $supervisor } finally {
                if ($null -ne $supervisor) { $supervisor.Dispose() }
            }
        }
        'down' {
            if ($null -ne (Invoke-Api '/processes' -Probe)) {
                $null = Invoke-Api '/project/stop' 'POST'
                $deadline = [DateTime]::UtcNow.AddSeconds(10)
                while ($null -ne (Invoke-Api '/processes' -Probe)) {
                    if ([DateTime]::UtcNow -ge $deadline) {
                        throw 'process-compose did not release its control port after shutdown'
                    }
                    Start-Sleep -Milliseconds 100
                }
            }
        }
        'restart' {
            $result = Invoke-Api '/project/configuration' 'POST' 60
            if ($null -ne $result -and $result.PSObject.Properties['proxy'] -and $result.proxy -eq 'updated') {
                Write-Output 'Reloaded process-compose.yaml; proxy restarted with the new configuration'
            } else {
                Write-Output 'Reloaded process-compose.yaml (proxy configuration unchanged)'
                $null = Invoke-Api '/process/restart/proxy' 'POST'
            }
            Wait-Ready
        }
        'status' { Invoke-Control @('process', 'list', '-o', 'wide') }
        'logs' {
            $follow = @($LogOptions | Where-Object { $_ -in @('-f', '--follow', '--follow=true') }).Count -gt 0
            Invoke-Control (@('process', 'logs', 'proxy', '--tail', '100') + $LogOptions) -Interactive:$follow
        }
        'tui' { Invoke-Control @('attach') -Interactive }
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
