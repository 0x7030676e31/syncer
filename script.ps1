# Check for administrative privileges
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")) {
    # Relaunch the script as administrator

    # Create a temporary script file
    $tempScriptPath = "$env:TEMP\temp_script.ps1"
    [IO.File]::WriteAllText($tempScriptPath, (Get-Content -Raw -Path $MyInvocation.MyCommand.Definition))

    # Relaunch the script from the temporary file
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$tempScriptPath`""
    Start-Process PowerShell -ArgumentList $arguments -Verb RunAs
    exit
}

# GitHub repository information
$repoOwner = "0x7030676e31"
$repoName = "syncer"

# GitHub API URL to fetch the latest release
$apiUrl = "https://api.github.com/repos/$repoOwner/$repoName/releases/latest"

# Fetch the latest release information
Write-Host "Fetching the latest release information from $apiUrl..."
try {
    $releaseInfo = Invoke-RestMethod -Uri $apiUrl -Headers @{ 'User-Agent' = 'PowerShellScript' }
    $latestTag = $releaseInfo.tag_name
    $assetUrl = $releaseInfo.assets | Where-Object { $_.name -match "client-windows-.*\.exe" } | Select-Object -ExpandProperty browser_download_url
} catch {
    Write-Error "Failed to fetch the latest release information: $_"
    exit 1
}

# Validate the asset URL
if (-not $assetUrl) {
    Write-Error "Could not find a matching client-windows-<hash>.exe asset in the latest release."
    exit 1
}

# Specify the destination path for the downloaded executable
$outputDirectory = "C:\ProgramData\CustomDownloads"
$outputPath = "$outputDirectory\client.exe"

# Ensure the destination directory exists
if (-not (Test-Path -Path $outputDirectory)) {
    Write-Host "Creating directory $outputDirectory..."
    New-Item -ItemType Directory -Path $outputDirectory -Force
}

# Download the executable
Write-Host "Downloading executable from $assetUrl to $outputPath..."
Invoke-WebRequest -Uri $assetUrl -OutFile $outputPath

# Check if the download was successful
if (-Not (Test-Path $outputPath)) {
    Write-Error "Failed to download the executable."
    exit 1
}

# Execute the downloaded executable with administrator permissions and wait for it to complete
Write-Host "Executing the downloaded executable and waiting for it to complete..."
$process = Start-Process -FilePath $outputPath -Verb RunAs -Wait -PassThru

# Check the exit code of the process
if ($process.ExitCode -ne 0) {
    Write-Error "The executable finished with a non-zero exit code: $($process.ExitCode)"
    exit $process.ExitCode
}

