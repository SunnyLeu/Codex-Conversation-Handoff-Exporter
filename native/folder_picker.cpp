// Small Windows Shell bridge. No application window/framework and no .NET.
// Interface declarations and GUIDs come from the installed Windows SDK.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <shobjidl.h>
#include <cwchar>
#include <initializer_list>

namespace {
class Apartment {
public:
    HRESULT status = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    ~Apartment() { if (SUCCEEDED(status)) CoUninitialize(); }
};
template<class T> class ComPtr {
public:
    T* value = nullptr;
    ~ComPtr() { if (value) value->Release(); }
    T** address() { return &value; }
    T* operator->() const { return value; }
};
HWND terminal_owner() {
    HWND console = GetConsoleWindow();
    if (console && IsWindowVisible(console)) return console;
    // GetConsoleWindow can be an invisible pseudoconsole window. Only borrow a
    // visible foreground terminal, never an unrelated application's window.
    HWND foreground = GetForegroundWindow();
    if (!foreground || !IsWindowVisible(foreground)) return nullptr;
    DWORD pid = 0;
    GetWindowThreadProcessId(foreground, &pid);
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (!process) return nullptr;
    wchar_t executable[32768];
    DWORD count = static_cast<DWORD>(sizeof(executable) / sizeof(wchar_t));
    BOOL ok = QueryFullProcessImageNameW(process, 0, executable, &count);
    CloseHandle(process);
    if (!ok) return nullptr;
    const wchar_t* base = std::wcsrchr(executable, L'\\');
    base = base ? base + 1 : executable;
    for (const wchar_t* name : {L"WindowsTerminal.exe", L"WindowsTerminalPreview.exe", L"OpenConsole.exe", L"conhost.exe"}) {
        if (_wcsicmp(base, name) == 0) return foreground;
    }
    return nullptr;
}
}
// 1 = selected; 0 = cancellation; negative HRESULT = failure.
// On success caller owns the UTF-16 path and frees it with codex_free_folder.
extern "C" int codex_pick_folder(wchar_t** result) noexcept {
    if (!result) return static_cast<int>(E_POINTER);
    *result = nullptr;
    Apartment apartment;
    if (FAILED(apartment.status)) return static_cast<int>(apartment.status);
    ComPtr<IFileOpenDialog> dialog;
    HRESULT hr = CoCreateInstance(CLSID_FileOpenDialog, nullptr, CLSCTX_INPROC_SERVER,
        IID_PPV_ARGS(dialog.address()));
    if (FAILED(hr)) return static_cast<int>(hr);
    FILEOPENDIALOGOPTIONS options;
    hr = dialog->GetOptions(&options);
    if (FAILED(hr)) return static_cast<int>(hr);
    hr = dialog->SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM |
        FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR | FOS_DONTADDTORECENT);
    if (FAILED(hr)) return static_cast<int>(hr);
    hr = dialog->SetTitle(L"選擇 Handoff 匯出資料夾");
    if (FAILED(hr)) return static_cast<int>(hr);
    // Intentionally no SetFolder/SetDefaultFolder: no program-chosen path.
    hr = dialog->Show(terminal_owner());
    if (hr == HRESULT_FROM_WIN32(ERROR_CANCELLED)) return 0;
    if (FAILED(hr)) return static_cast<int>(hr);
    ComPtr<IShellItem> selected;
    hr = dialog->GetResult(selected.address());
    if (FAILED(hr)) return static_cast<int>(hr);
    hr = selected->GetDisplayName(SIGDN_FILESYSPATH, result);
    if (FAILED(hr)) return static_cast<int>(hr);
    return 1;
}
extern "C" void codex_free_folder(wchar_t* value) noexcept { CoTaskMemFree(value); }
