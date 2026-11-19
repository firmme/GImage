function Open-UpdateResponse([string]$url, [int]$timeout) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $request = [Net.HttpWebRequest]::Create($url)
    $request.UserAgent = 'GImage Updater'
    $request.Accept = 'application/vnd.github+json, application/octet-stream'
    $request.Timeout = $timeout
    $request.ReadWriteTimeout = 20000
    $request.MaximumAutomaticRedirections = 5
    $response = $request.GetResponse()
    if ($response.ResponseUri.Scheme -ne 'https') {
        $response.Dispose()
        throw 'Update response must use HTTPS.'
    }
    $response
}
