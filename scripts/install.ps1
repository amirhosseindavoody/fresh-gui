# fresh-gui installer for Windows (PowerShell 5.1+).
#
# From an elevated or normal PowerShell prompt:
#
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | iex"
#
# Review first:
#
#   powershell -c "irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | more"
#
# The scriptblock below is what `irm | iex` runs. Named parameters also work
# when the file itself is the entry point:
#
#   powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -FreshGuiVersion 2026.921.5 -NoPathUpdate
#
# Environment (applied after parameters, so they work with irm | iex):
#   FRESH_GUI_VERSION         latest (default) or CalVer (2026.921.5 / v2026.921.5)
#   FRESH_GUI_HOME            install prefix (default: %USERPROFILE%\.fresh-gui)
#   FRESH_GUI_BIN_DIR         binaries directory (default: %FRESH_GUI_HOME%\bin)
#   FRESH_GUI_REPOURL         GitHub repository
#   FRESH_GUI_NO_PATH_UPDATE  any non-empty value skips the user PATH update
#   FRESH_GUI_COMPONENTS      both (default) | client | daemon
#   FRESH_GUI_DRY_RUN         any non-empty value prints the plan and exits

# Parse -File arguments here. `irm | iex` has no script-level param block
# (Invoke-Expression rejects one), and a bare script stores unknown args in $args.
$script:FreshGuiFileArgs = @{}
$entryScript = ''
if ($PSCommandPath) {
    $entryScript = Split-Path -Leaf $PSCommandPath
}
if ($entryScript -eq 'install.ps1') {
    $pending = $null
    foreach ($arg in $args) {
        if ($null -ne $pending) {
            $script:FreshGuiFileArgs[$pending] = $arg
            $pending = $null
            continue
        }
        switch ($arg) {
            '-FreshGuiVersion' { $pending = 'FreshGuiVersion' }
            '-FreshGuiHome' { $pending = 'FreshGuiHome' }
            '-FreshGuiRepourl' { $pending = 'FreshGuiRepourl' }
            '-FreshGuiComponents' { $pending = 'FreshGuiComponents' }
            '-NoPathUpdate' { $script:FreshGuiFileArgs['NoPathUpdate'] = $true }
            '-DryRun' { $script:FreshGuiFileArgs['DryRun'] = $true }
            default { throw "Unknown argument: $arg" }
        }
    }
    if ($null -ne $pending) {
        throw "Missing value for -$pending"
    }
}

& {
    param(
        [string] $FreshGuiVersion = 'latest',
        [string] $FreshGuiHome = '',
        [switch] $NoPathUpdate,
        [string] $FreshGuiRepourl = 'https://github.com/amirhosseindavoody/fresh-gui',
        [string] $FreshGuiComponents = 'both',
        [switch] $DryRun
    )

    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    function Mask-Credentials {
        param([string] $Url)
        return $Url -replace '://[^:@/]+:[^@/]+@', '://***:***@'
    }

    function Publish-Env {
        if (-not ('Win32.NativeMethods' -as [type])) {
            Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
        }

        $result = [UIntPtr]::Zero
        [Win32.NativeMethods]::SendMessageTimeout(
            [IntPtr] 0xffff,
            0x1a,
            [UIntPtr]::Zero,
            'Environment',
            2,
            5000,
            [ref] $result
        ) | Out-Null
    }

    function Write-Env {
        param(
            [string] $Name,
            [string] $Value
        )

        $registerKey = Get-Item -Path 'HKCU:'
        $envKey = $registerKey.OpenSubKey('Environment', $true)
        if ($null -eq $envKey) {
            throw 'Could not open HKCU:\Environment for the user PATH update.'
        }
        $kind = [Microsoft.Win32.RegistryValueKind]::String
        if ($Value.Contains('%')) {
            $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        } elseif ($null -ne $envKey.GetValue($Name)) {
            $kind = $envKey.GetValueKind($Name)
        }
        $envKey.SetValue($Name, $Value, $kind)
        Publish-Env
    }

    function Get-EnvValue {
        param([string] $Name)

        $registerKey = Get-Item -Path 'HKCU:'
        $envKey = $registerKey.OpenSubKey('Environment')
        if ($null -eq $envKey) {
            return $null
        }
        $option = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
        return $envKey.GetValue($Name, $null, $option)
    }

    function Get-TargetTriple {
        try {
            $assembly = [System.Reflection.Assembly]::LoadWithPartialName('System.Runtime.InteropServices.RuntimeInformation')
            if ($null -eq $assembly) {
                throw 'RuntimeInformation assembly is unavailable.'
            }
            $type = $assembly.GetType('System.Runtime.InteropServices.RuntimeInformation')
            $property = $type.GetProperty('OSArchitecture')
            switch ($property.GetValue($null).ToString()) {
                'X64' { return 'x86_64-pc-windows-msvc' }
                'X86' { return 'i686-pc-windows-msvc' }
                'Arm' { return 'thumbv7a-pc-windows-msvc' }
                'Arm64' { return 'aarch64-pc-windows-msvc' }
            }
        } catch {
            Write-Verbose "Get-TargetTriple failed: $_"
        }

        if ([System.Environment]::Is64BitOperatingSystem) {
            return 'x86_64-pc-windows-msvc'
        }
        return 'i686-pc-windows-msvc'
    }

    function Get-HttpStatusCode {
        param($ErrorRecord)

        $candidates = @()
        if ($ErrorRecord.Exception) {
            $candidates += $ErrorRecord.Exception
            if ($ErrorRecord.Exception.InnerException) {
                $candidates += $ErrorRecord.Exception.InnerException
            }
        }
        foreach ($ex in $candidates) {
            $response = $null
            if ($ex.PSObject.Properties.Name -contains 'Response') {
                $response = $ex.Response
            }
            if ($null -ne $response -and ($response.PSObject.Properties.Name -contains 'StatusCode')) {
                return [int] $response.StatusCode
            }
        }
        if ($ErrorRecord.Exception) {
            $message = [string] $ErrorRecord.Exception.Message
            if ($message -match '\(404\)') {
                return 404
            }
        }
        return 0
    }

    function Invoke-FreshGuiDownload {
        param(
            [string] $Url,
            [string] $Destination
        )

        $params = @{
            Uri             = $Url
            OutFile         = $Destination
            UseBasicParsing = $true
            UserAgent       = 'fresh-gui-install'
        }
        Invoke-WebRequest @params
    }

    function Test-ZipMagic {
        param([string] $Path)
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        try {
            $first = $stream.ReadByte()
            $second = $stream.ReadByte()
            return ($first -eq 80 -and $second -eq 75)
        } finally {
            $stream.Dispose()
        }
    }

    function Expand-FreshGuiArchive {
        param(
            [string] $Archive,
            [string] $Destination
        )
        if (-not (Test-Path -LiteralPath $Destination)) {
            New-Item -ItemType Directory -Path $Destination | Out-Null
        }
        # Published Windows assets are often GNU tar files with a .zip name.
        if (Test-ZipMagic $Archive) {
            Expand-Archive -LiteralPath $Archive -DestinationPath $Destination -Force
            return
        }
        $tar = Get-Command tar -ErrorAction SilentlyContinue
        if (-not $tar) {
            throw "Archive is a tar file named .zip (the published Windows release layout). tar is required to extract it. Windows 10 and later include tar.exe."
        }
        & $tar.Source -xf $Archive -C $Destination
        if ($LASTEXITCODE -ne 0) {
            throw "tar failed to extract $(Mask-Credentials $Archive) (exit $LASTEXITCODE)."
        }
    }

    function Test-Sha256Hex {
        param([string] $Value)
        return $Value -match '^[0-9a-f]{64}$'
    }

    function Install-Component {
        param(
            [string] $Url,
            [string] $Work,
            [string] $BinaryName,
            [string] $BinDir,
            [switch] $Optional
        )

        New-Item -ItemType Directory -Path $Work | Out-Null
        $archive = Join-Path $Work 'archive.zip'
        try {
            Invoke-FreshGuiDownload -Url $Url -Destination $archive
        } catch {
            $status = Get-HttpStatusCode $_
            $shown = Mask-Credentials $Url
            if ($status -eq 404 -and $Optional) {
                Write-Warning "Not in this release, skipping: $shown"
                return $false
            }
            if ($status -ne 0) {
                throw "Download failed (HTTP $status): $shown"
            }
            throw "Download failed: $shown. $($_.Exception.Message)"
        }
        if (-not (Test-Path -LiteralPath $archive) -or ((Get-Item -LiteralPath $archive).Length -lt 1)) {
            throw "Downloaded file is empty: $(Mask-Credentials $Url)"
        }

        $sumUrl = "$Url.sha256"
        $sumFile = Join-Path $Work 'archive.zip.sha256'
        $haveSum = $true
        try {
            Invoke-FreshGuiDownload -Url $sumUrl -Destination $sumFile
        } catch {
            $status = Get-HttpStatusCode $_
            if ($status -eq 404) {
                Write-Warning "No checksum asset ($(Mask-Credentials $sumUrl)); continuing without verification."
                $haveSum = $false
            } else {
                $shown = Mask-Credentials $sumUrl
                if ($status -ne 0) {
                    throw "Checksum download failed (HTTP $status): $shown"
                }
                throw "Checksum download failed: $shown. $($_.Exception.Message)"
            }
        }

        if ($haveSum) {
            $line = (Get-Content -LiteralPath $sumFile -TotalCount 1)
            $expected = ''
            if ($line -match '([A-Fa-f0-9]{64})') {
                $expected = $Matches[1].ToLowerInvariant()
            }
            if (-not (Test-Sha256Hex $expected)) {
                throw "Checksum file does not contain a SHA-256 hex digest: $(Mask-Credentials $sumUrl)"
            }
            $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
            if ($actual -ne $expected) {
                throw "Checksum mismatch for $(Mask-Credentials $sumUrl). Expected $expected, actual $actual."
            }
            Write-Host 'Checksum verified.'
        }

        $extract = Join-Path $Work 'extract'
        Expand-FreshGuiArchive -Archive $archive -Destination $extract
        $found = @(Get-ChildItem -LiteralPath $extract -Recurse -File -Filter $BinaryName)
        if ($found.Count -lt 1) {
            throw "Archive does not contain '$BinaryName'."
        }
        $destination = Join-Path $BinDir $BinaryName
        Copy-Item -LiteralPath $found[0].FullName -Destination $destination -Force
        Write-Host "Installed $destination"
        return $true
    }

    if ($env:OS -ne 'Windows_NT') {
        throw 'install.ps1 targets Windows. On Linux, run scripts/install.sh.'
    }

    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    } catch {
        Write-Verbose "Could not set TLS 1.2 explicitly: $_"
    }

    if ($env:FRESH_GUI_VERSION) {
        $FreshGuiVersion = $env:FRESH_GUI_VERSION
    }
    if ($env:FRESH_GUI_HOME) {
        $FreshGuiHome = $env:FRESH_GUI_HOME
    }
    if (-not $FreshGuiHome) {
        if (-not $env:USERPROFILE) {
            throw 'USERPROFILE is not set. Pass -FreshGuiHome or set FRESH_GUI_HOME.'
        }
        $FreshGuiHome = Join-Path $env:USERPROFILE '.fresh-gui'
    }
    if ($env:FRESH_GUI_NO_PATH_UPDATE) {
        $NoPathUpdate = $true
    }
    if ($env:FRESH_GUI_REPOURL) {
        $FreshGuiRepourl = $env:FRESH_GUI_REPOURL
    }
    if ($env:FRESH_GUI_COMPONENTS) {
        $FreshGuiComponents = $env:FRESH_GUI_COMPONENTS
    }
    if ($env:FRESH_GUI_DRY_RUN) {
        $DryRun = $true
    }

    $FreshGuiRepourl = $FreshGuiRepourl.TrimEnd('/')

    $components = $FreshGuiComponents.ToLowerInvariant()
    $wantClient = $false
    $wantDaemon = $false
    if ($components -eq 'both' -or $components -eq 'all' -or $components -eq '') {
        $wantClient = $true
        $wantDaemon = $true
    } elseif ($components -eq 'client' -or $components -eq 'app' -or $components -eq 'gui') {
        $wantClient = $true
    } elseif ($components -eq 'daemon' -or $components -eq 'server' -or $components -eq 'backend') {
        $wantDaemon = $true
    } else {
        throw "FRESH_GUI_COMPONENTS must be both, client, or daemon (got '$FreshGuiComponents')."
    }

    $target = Get-TargetTriple
    if ($target -ne 'x86_64-pc-windows-msvc') {
        throw "Unsupported Windows architecture '$target'. fresh-gui publishes x86_64-pc-windows-msvc binaries."
    }

    if ($FreshGuiVersion -eq 'latest') {
        $latestUrl = "$FreshGuiRepourl/releases/latest"
        $request = [System.Net.HttpWebRequest] [System.Net.WebRequest]::Create($latestUrl)
        $request.AllowAutoRedirect = $true
        $request.MaximumAutomaticRedirections = 5
        $request.UserAgent = 'fresh-gui-install'
        $request.Method = 'GET'
        $response = $request.GetResponse()
        try {
            $finalUrl = $response.ResponseUri.AbsoluteUri
            $FreshGuiVersion = $null
            if ($finalUrl -match '/releases/tag/v([^/?#]+)') {
                $FreshGuiVersion = $Matches[1]
            } else {
                $reader = New-Object System.IO.StreamReader($response.GetResponseStream())
                try {
                    $body = $reader.ReadToEnd()
                } finally {
                    $reader.Close()
                }
                if ($body -match '/releases/tag/v([A-Za-z0-9._+-]+)') {
                    $FreshGuiVersion = $Matches[1]
                }
            }
        } finally {
            $response.Close()
        }
        if (-not $FreshGuiVersion) {
            throw "Could not resolve the latest release from $latestUrl (landed on $finalUrl). Set FRESH_GUI_VERSION."
        }
    } else {
        $FreshGuiVersion = $FreshGuiVersion -replace '^v', ''
    }
    if ($FreshGuiVersion -notmatch '^[A-Za-z0-9._+-]+$') {
        throw "Invalid FRESH_GUI_VERSION '$FreshGuiVersion'."
    }

    $binDir = if ($env:FRESH_GUI_BIN_DIR) { $env:FRESH_GUI_BIN_DIR } else { Join-Path $FreshGuiHome 'bin' }

    $clientUrl = $null
    $daemonUrl = $null
    if ($wantClient) {
        $clientUrl = "$FreshGuiRepourl/releases/download/v$FreshGuiVersion/fresh-gui-client-$FreshGuiVersion-$target.zip"
    }
    if ($wantDaemon) {
        $daemonUrl = "$FreshGuiRepourl/releases/download/v$FreshGuiVersion/fresh-gui-$FreshGuiVersion-$target.zip"
    }

    Write-Host "This script will download and install fresh-gui ($FreshGuiVersion)."
    Write-Host "Binaries will be installed into '$binDir'"
    if ($clientUrl) {
        Write-Host ("Client: " + (Mask-Credentials $clientUrl))
    }
    if ($daemonUrl) {
        Write-Host ("Daemon: " + (Mask-Credentials $daemonUrl))
    }
    if ($DryRun) {
        Write-Host 'Dry run: no files will be written.'
        return
    }

    if (-not (Test-Path -LiteralPath $binDir)) {
        New-Item -ItemType Directory -Path $binDir | Out-Null
    }

    # Default installs both when the release has both archives. A 404 on one
    # of them skips that piece. Asking for only client or only daemon fails
    # if that archive is missing.
    $optional = $wantClient -and $wantDaemon
    $gotClient = $false
    $gotDaemon = $false
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ('fresh-gui-install-' + [guid]::NewGuid().ToString('n'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        if ($wantClient) {
            if (Install-Component -Url $clientUrl -Work (Join-Path $work 'client') -BinaryName 'fresh-gui-app.exe' -BinDir $binDir -Optional:$optional) {
                $gotClient = $true
            }
        }
        if ($wantDaemon) {
            if (Install-Component -Url $daemonUrl -Work (Join-Path $work 'daemon') -BinaryName 'fresh-gui.exe' -BinDir $binDir -Optional:$optional) {
                $gotDaemon = $true
            }
        }
    } finally {
        if (Test-Path -LiteralPath $work) {
            Remove-Item -LiteralPath $work -Recurse -Force
        }
    }
    if (-not $gotClient -and -not $gotDaemon) {
        throw "No fresh-gui archive from this release could be installed."
    }

    Write-Host "Installed into '$binDir'."

    if ($NoPathUpdate) {
        Write-Host 'No PATH update because -NoPathUpdate / FRESH_GUI_NO_PATH_UPDATE is set.'
        Write-Host "Add '$binDir' to your user PATH to run fresh-gui-app and fresh-gui."
    } else {
        $current = Get-EnvValue -Name 'PATH'
        if ($null -eq $current) {
            $current = ''
        }
        $pattern = '*' + $binDir + '*'
        if ($current -like $pattern) {
            Write-Host "'$binDir' is already in the user PATH."
        } else {
            Write-Host "Adding '$binDir' to the user PATH."
            if ($current) {
                Write-Env -Name 'PATH' -Value ($binDir + ';' + $current)
            } else {
                Write-Env -Name 'PATH' -Value $binDir
            }
            $env:PATH = $binDir + ';' + $env:PATH
            Write-Host 'Open a new terminal before using fresh-gui.'
        }
    }

    Write-Host ''
    if ($gotClient) {
        Write-Host 'Next, open a project on a Linux machine:'
        Write-Host '  fresh-gui-app remote add lab user@server --root /path/to/project'
        Write-Host '  fresh-gui-app remote connect lab'
    }
    if ($gotDaemon) {
        Write-Host 'Or start the daemon in a project directory on this machine:'
        Write-Host '  cd C:\path\to\project'
        Write-Host '  fresh-gui'
    }
} @script:FreshGuiFileArgs
