function Get-UpdateSha256([string]$path) {
    $stream = [IO.File]::OpenRead($path)
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        [BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '')
    } finally {
        $sha256.Dispose()
        $stream.Dispose()
    }
}
