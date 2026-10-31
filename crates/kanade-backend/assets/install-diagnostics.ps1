# Inspect only types, numeric codes and fixed PowerShell categories. Never
# serialize exception messages, source lines, targets or invocation arguments.
function Get-InstallErrorKind($record, [string]$stage) {
    $exception = $record.Exception
    while ($null -ne $exception) {
        if ($exception -is [System.UnauthorizedAccessException] -or $exception -is [System.Security.SecurityException]) { return 'permission_denied' }
        if ($exception -is [System.IO.FileNotFoundException] -or $exception -is [System.IO.DirectoryNotFoundException]) { return 'file_not_found' }
        if ($exception -is [System.ComponentModel.Win32Exception]) {
            switch ($exception.NativeErrorCode) {
                1223 { return 'elevation_cancelled' }
                5 { return 'permission_denied' }
                2 { return 'file_not_found' }
                3 { return 'file_not_found' }
            }
        }
        if ($exception -is [System.IO.InvalidDataException]) { return 'invalid_archive' }
        if ($exception -is [System.IO.IOException]) { return 'io_error' }
        $exception = $exception.InnerException
    }
    switch ([string]$record.CategoryInfo.Category) {
        'PermissionDenied' { return 'permission_denied' }
        'ObjectNotFound' { return 'file_not_found' }
    }
    if ($record.CategoryInfo.Activity -in @('Start-Service', 'Stop-Service', 'New-Service', 'Get-Service') -or $stage -eq 'service_start') { return 'service_error' }
    if ($stage -eq 'extract') { return 'invalid_archive' }
    if ($stage -eq 'download') { return 'http_error' }
    if ($stage -eq 'connection_check') { return 'connection_failed' }
    return 'command_failed'
}
