// Staap — WinUI 3 shell over the core C ABI (issue #64).
//
// Dumb renderer over the core run registry: the core owns the roster,
// selection, filter, PTYs, statuses, key table, feed reconciler, and
// persistence — this shell owns WinUI controls, event wiring, clipboard
// access, and byte transport only.
//
// Parity wiring (same contract as the macOS/Linux shells):
//   roster      core registry rows, grouped Working → Idle
//               (needs-input shares Idle, issue #105) → History, one
//               shared selection across lists (#73)
//   spawn       single New Session button / Ctrl+N / Ctrl+Shift+N ->
//               in-place new-session screen on the right side (machine
//               dropdown + SSH add dialog + remote folder browser +
//               folder x CLI + yolo); Start routes to bridge_run_spawn
//               or bridge_run_spawn_remote (blank folder = remote login
//               dir). No repeat-last: every press opens the screen
//               fresh; the old modal ContentDialog is gone.
//               Start/Cancel close the screen; restart/close stay
//               keyboard-only.
//   restart     Ctrl+R -> bridge_run_restart (same id, keeps title/links)
//   close       Ctrl+W -> bridge_run_close + autosave (no confirm)
//   converse    key event -> bridge_key_encode -> bridge_run_write;
//               pump -> bridge_feed_delta -> append
//   copy/paste  native TextBox selection + Ctrl+Shift+C; Ctrl+V pastes via
//               Clipboard -> bridge_run_write; Ctrl+C forwards ETX
//   scroll      output TextBox in a ScrollViewer, per-run text retained
//   search      sidebar search box (core-owned filter) filters every group
//   history     collapsed-by-default group of rows with no live PTY
//               (core-owned membership + expansion, persisted),
//               restored every launch; ended rows offer restart inline
//   persist     automatic (throttled pump autosave + close hook)
//
// ConPTY note: no console is ever created on the WinUI side. The core's
// EmbeddedPty on Windows is ConPTY-backed (portable-pty uses the native
// Console Pseudo-terminal API), so bridge_run_spawn IS the ConPTY spawn;
// the WinUI surface renders core snapshots through feed deltas. Exactly
// one line discipline (the core's) exists, so nothing double-echoes —
// the same single-emulator rule as the Linux shell's "no PTY inside VTE".

#include "pch.h"
#include "MainWindow.xaml.h"
#if __has_include("MainWindow.g.cpp")
#include "MainWindow.g.cpp"
#endif

#include "core_bridge.h"
#include "json_mini.h"
#include "picker.h"

#include <memory>
#include <thread>

#include <shobjidl.h> // IInitializeWithWindow (folder picker owner)
#include <microsoft.ui.xaml.window.h> // IWindowNative (picker owner HWND)

#include <utility>

using namespace winrt;
using namespace Microsoft::UI::Xaml;
using namespace Microsoft::UI::Xaml::Controls;
using namespace Microsoft::UI::Xaml::Input;
using namespace Windows::Foundation;

namespace winrt::StaapWinUI::implementation
{
    /* Dumb renderer over the core run registry: rows, selection, filter,
     * live PTYs, statuses, links, key bytes, feed bytes, and persistence
     * all come from the core — this shell owns WinUI controls, event
     * wiring, clipboard access, and byte transport only.
     *
     * Roster/status refresh rides on the same 50ms pump tick as the
     * Swift and GTK shells. Fixed spawn grid: there is no backing
     * widget grid to measure (the surface is snapshot-fed), so spawns
     * and resizes use the classic 80x25. */
    constexpr int kPumpMs = 50;
    constexpr unsigned kCols = 80;
    constexpr unsigned kRows = 25;
    /* Bounded per-run output (local-only trust + bounded growth: an
     * accumulate-forever buffer would leak memory over long runs). */
    constexpr std::size_t kShownCap = 100000;
    /* Resizable sidebar: the Thumb between the roster card and the
     * terminal card drives SidebarColumn (the XAML default is 320px;
     * clamped to the GTK shell's 220px floor and a 480px ceiling so
     * long titles stay glanceable). The width persists in
     * LocalSettings (local-only trust: plain local store, no
     * account, no sync). */
    /* Shared sidebar bounds/step live in the core (`staap_clamp_sidebar`
     * + `bridge constants`); the XAML pins the same range declaratively.
     * Local copies only feed the restore fallback below. */
    constexpr double kSidebarMin = 220.0;
    constexpr double kSidebarMax = 480.0;
    constexpr double kSidebarKeyStep = 8.0;
    constexpr wchar_t const *kSidebarWidthKey = L"SidebarWidth";
    /* Shell-local last SSH host (LocalSettings; the C ABI exposes no
     * last-host getter, so this never leaves the shell). */
    constexpr wchar_t const *kRemoteHostKey = L"RemoteHost";

    static std::wstring to_wide(std::string const &s) {
        if (s.empty()) {
            return {};
        }
        int n = MultiByteToWideChar(
            CP_UTF8, 0, s.c_str(), static_cast<int>(s.size()), nullptr, 0);
        std::wstring w(static_cast<std::size_t>(n), L'\0');
        MultiByteToWideChar(CP_UTF8, 0, s.c_str(),
                            static_cast<int>(s.size()), w.data(), n);
        return w;
    }

    static std::string to_utf8(hstring const &s) {
        auto view = s.c_str();
        int n = WideCharToMultiByte(CP_UTF8, 0, view, -1, nullptr, 0, nullptr,
                                    nullptr);
        if (n <= 1) {
            return {};
        }
        std::string out(static_cast<std::size_t>(n - 1), '\0');
        WideCharToMultiByte(CP_UTF8, 0, view, -1, out.data(), n, nullptr,
                            nullptr);
        return out;
    }

    /* Non-color status marker via the core (single copy of the glyph
     * rule: ●/◐/○, never color-only). */
    static hstring status_glyph(int st) {
        char *g = bridge_status_glyph(st);
        std::string narrow = g ? g : "";
        bridge_string_free(g);
        return hstring{to_wide(narrow) + L" "};
    }

    /* Resolve a printable UTF-32 code point for a virtual key under the
     * current thread layout (user32; a WinUI 3 desktop app may call it
     * freely). Ctrl+letter bypasses this: with Control held ToUnicode
     * returns control characters, but the encoder wants the letter. */
    static char32_t key_char(int vk, bool ctrl) {
        if (ctrl) {
            if (vk >= 'A' && vk <= 'Z') {
                return static_cast<char32_t>(vk);
            }
            return 0;
        }
        BYTE state[256]{};
        if (!GetKeyboardState(state)) {
            return 0;
        }
        UINT sc = MapVirtualKeyW(static_cast<UINT>(vk), MAPVK_VK_TO_VSC);
        WCHAR buf[8]{};
        int n = ToUnicode(static_cast<UINT>(vk), sc, state, buf, 8, 0);
        if (n == 1) {
            return static_cast<char32_t>(buf[0]);
        }
        /* Dead key or multi-char composition: leave to the control. */
        return 0;
    }

    MainWindow::MainWindow() {
        InitializeComponent();
        /* Native Windows 11 chrome (issue #72): Mica system backdrop,
         * content extended into the title bar with AppTitleBar as the
         * drag region, transparent caption-button wells so Mica shows
         * through. Same controls and handlers: no behavior change. */
        SystemBackdrop(Media::MicaBackdrop{});
        ExtendsContentIntoTitleBar(true);
        SetTitleBar(AppTitleBar());
        try {
            auto titleBar = AppWindow().TitleBar();
            titleBar.ButtonBackgroundColor(
                Microsoft::UI::Colors::Transparent());
            titleBar.ButtonInactiveBackgroundColor(
                Microsoft::UI::Colors::Transparent());
        } catch (...) {
            /* Pre-Windows 11 host: chrome stays default. */
        }
        m_core = bridge_core_new();

        /* Restore the persisted sidebar width (LocalSettings is a
         * plain local store — local-only trust: no account, no
         * sync). Out-of-range values fall back to the 320px XAML
         * default. */
        try {
            auto values = Windows::Storage::ApplicationData::Current()
                              .LocalSettings()
                              .Values();
            if (values.HasKey(kSidebarWidthKey)) {
                double w =
                    unbox_value<double>(values.Lookup(kSidebarWidthKey));
                if (w >= kSidebarMin && w <= kSidebarMax) {
                    SidebarColumn().Width(GridLengthHelper::FromPixels(w));
                }
            }
        } catch (...) {
        }

        m_timer = DispatcherQueue().CreateTimer();
        m_timer.Interval(
            std::chrono::milliseconds(kPumpMs));
        m_timer.Tick({this, &MainWindow::OnTick});
        m_timer.Start();

        m_closedToken = Closed({this, &MainWindow::OnClosed});
        RefreshRoster();
        ShowSelected();
        /* No idle banner: the status line stays empty until a real
         * failure needs it (macOS parity). */
    }

    MainWindow::~MainWindow() {
        if (m_timer) {
            m_timer.Stop();
        }
        /* Core registry owns the PTYs (freed with the core handle). */
        m_rows.clear();
        bridge_core_free(m_core);
        m_core = nullptr;
    }

    void MainWindow::SetStatus(hstring const &text) {
        StatusText().Text(text);
    }

    /* Selected roster row id, from the core selection. */
    std::wstring MainWindow::SelectedId() {
        if (!m_core) {
            return {};
        }
        size_t sel = bridge_selected(m_core);
        char *json = bridge_session_json(m_core, sel);
        std::string id = json ? staapjson::get_string(json ? json : "", "id") : "";
        bridge_string_free(json);
        return to_wide(id);
    }

    bool MainWindow::SelectedIsLive() {
        std::wstring id = SelectedId();
        if (id.empty()) {
            return false;
        }
        return bridge_is_live(m_core, to_utf8(hstring{id}).c_str()) != 0;
    }

    void MainWindow::ForwardBytes(char const *data, std::size_t len) {
        std::wstring wid = SelectedId();
        if (wid.empty()) {
            return;
        }
        std::string id = to_utf8(hstring{wid});
        if (!bridge_is_live(m_core, id.c_str())) {
            return;
        }
        char *err = nullptr;
        if (bridge_run_write(m_core, id.c_str(),
                             reinterpret_cast<unsigned char const *>(data),
                             len, &err) != 0) {
            std::string msg = "Could not send input: ";
            msg += err ? err : "unknown error";
            SetStatus(to_hstring(msg));
            free(err);
        }
    }

    /* Rebuild the grouped roster only when the fingerprint (row count
     * + filter + core selection + per-row status + per-row live-ness)
     * changes; ticks otherwise leave the selection alone. Rows,
     * selection, filter, and live-ness all come from the core registry —
     * spawning attaches a real row, so every run renders in the roster
     * (no shell-local terminals). Group order is fixed (issue #73):
     * Working, Idle (needs-input shares Idle, #105), History — every row
     * still shows its core status glyph, title, and project/harness.
     * Restored every launch by staap_core_new. */
    void MainWindow::RefreshRoster() {
        if (!m_core) {
            return;
        }
        using Rows = std::vector<std::pair<std::wstring, std::wstring>>;
        size_t n = bridge_session_count(m_core);
        Rows idle_attn;
        Rows working;
        Rows idle;
        Rows history;
        std::string fingerprint;
        fingerprint += std::to_string(n);
        fingerprint += '|';
        fingerprint += m_filter;
        fingerprint += '|';
        fingerprint += std::to_string(bridge_selected(m_core));
        fingerprint += '|';
        fingerprint += bridge_history_expanded(m_core) ? 'E' : 'e';
        fingerprint += '|';
        for (size_t i = 0; i < n; ++i) {
            char *json = bridge_session_json(m_core, i);
            std::string js = json ? json : "";
            bridge_string_free(json);
            int st = bridge_status(m_core, i);
            std::string id = staapjson::get_string(js, "id");
            if (id.empty()) {
                continue;
            }
            std::wstring wid = to_wide(id);
            /* Live-ness comes from the core registry (spawning moves a
             * row from history to a live group inside the core). */
            bool live = bridge_is_live(m_core, id.c_str()) != 0;
            fingerprint += std::to_string(st);
            fingerprint += live ? 'L' : 'h';
            fingerprint += ';';
            /* Core-owned filter rule (title/project/id, not the raw
             * JSON blob the old shell matched). */
            if (!bridge_row_matches(m_core, i, m_filter.c_str())) {
                continue;
            }
            std::string title = staapjson::get_string(js, "title");
            std::string project = staapjson::get_string(js, "project");
            std::string harness = staapjson::get_string(js, "harness");
            std::string line = (title.empty() ? id : title);
            if (!project.empty()) {
                line += " — " + project;
            }
            if (!harness.empty()) {
                line += " · " + harness;
            }
            std::pair<std::wstring, std::wstring> row{
                wid, std::wstring(status_glyph(st)) + to_wide(line)};
            if (!live) {
                history.push_back(std::move(row));
            } else if (st == STAAP_STATUS_ATTENTION) {
                idle_attn.push_back(std::move(row));
            } else if (st == STAAP_STATUS_WORKING) {
                working.push_back(std::move(row));
            } else {
                idle.push_back(std::move(row));
            }
        }
        /* Issue #105: idle and needs-input share one section, with
         * the attention rows first. */
        for (auto &r : idle) {
            idle_attn.push_back(std::move(r));
        }
        idle = std::move(idle_attn);

        if (fingerprint == m_fingerprint) {
            return;
        }
        m_fingerprint = fingerprint;
        char *work_hdr = bridge_section_title(STAAP_STATUS_WORKING);
        char *idle_hdr = bridge_section_title(STAAP_STATUS_IDLE);
        /* Issue #105: the Needs-input group folds into Idle. */
        NeedsHeader().Visibility(Visibility::Collapsed);
        NeedsInputList().Visibility(Visibility::Collapsed);
        WorkingHeader().Text(winrt::hstring(
            to_wide(work_hdr ? work_hdr : "Working") + L" (" +
            std::to_wstring(working.size()) + L")"));
        IdleHeader().Text(winrt::hstring(
            to_wide(idle_hdr ? idle_hdr : "Idle") + L" (" +
            std::to_wstring(idle.size()) + L")"));
        bridge_string_free(work_hdr);
        bridge_string_free(idle_hdr);
        HistoryExpander().Header(box_value(winrt::hstring(
            L"History (" + std::to_wstring(history.size()) + L")")));
        m_syncing = true;
        /* Sync the expander to the core-owned expansion state (collapsed
         * by default); the toggle handler below ignores programmatic
         * moves via m_syncing so syncing never writes back. */
        HistoryExpander().IsExpanded(bridge_history_expanded(m_core) != 0);
        RebuildGroupList(WorkingList(), working);
        RebuildGroupList(IdleList(), idle);
        RebuildGroupList(HistoryList(), history);
        m_syncing = false;
        /* Restore the core selection into the lists. */
        std::wstring sel = SelectedId();
        if (!sel.empty()) {
            SelectRowById(sel);
        } else {
            std::wstring first;
            if (FirstRowId(first)) {
                SelectRowById(first);
            }
        }
    }

    /* Repopulate one group list from (id, display) rows. Runs under
     * m_syncing from RefreshRoster, so the clear/repopulate
     * SelectionChanged fan-out is ignored. */
    void MainWindow::RebuildGroupList(
        ListView const &list,
        std::vector<std::pair<std::wstring, std::wstring>> const &rows) {
        list.Items().Clear();
        for (auto const &row : rows) {
            ListViewItem item;
            item.Content(box_value(row.second));
            item.Tag(box_value(row.first));
            list.Items().Append(item);
        }
    }

    /* Move the shared selection to the row with this id, clearing the
     * other three lists. Writes through to the core selection so the
     * pump, the detail pane, and the lists never disagree. No-op when no
     * list holds the id. */
    void MainWindow::SelectRowById(std::wstring const &id) {
        ListView lists[] = {NeedsInputList(), IdleList(), WorkingList(),
                            HistoryList()};
        bool found = false;
        m_syncing = true;
        for (auto const &list : lists) {
            auto items = list.Items();
            int at = -1;
            for (uint32_t i = 0; i < items.Size(); ++i) {
                auto item = items.GetAt(i).try_as<ListViewItem>();
                if (item &&
                    std::wstring(unbox_value<hstring>(item.Tag())) == id) {
                    at = static_cast<int>(i);
                    found = true;
                    break;
                }
            }
            list.SelectedIndex(at);
        }
        m_syncing = false;
        if (found) {
            /* Mirror into the core selection (find the row index by id). */
            size_t n = bridge_session_count(m_core);
            for (size_t i = 0; i < n; ++i) {
                char *json = bridge_session_json(m_core, i);
                std::string rid =
                    staapjson::get_string(json ? json : "", "id");
                bridge_string_free(json);
                if (to_wide(rid) == id) {
                    bridge_select(m_core, i);
                    break;
                }
            }
            ShowSelected();
        }
    }

    /* First row id across the groups in display order; false when every
     * list is empty (no runs yet, or the search matches nothing). */
    bool MainWindow::FirstRowId(std::wstring &id) {
        ListView lists[] = {NeedsInputList(), IdleList(), WorkingList(),
                            HistoryList()};
        for (auto const &list : lists) {
            auto items = list.Items();
            if (items.Size() == 0) {
                continue;
            }
            auto first = items.GetAt(0).try_as<ListViewItem>();
            if (first) {
                id = std::wstring(unbox_value<hstring>(first.Tag()));
                return true;
            }
        }
        return false;
    }

    /* Index of the roster row with this id, or -1 (stale selections
     * have no roster row). */
    long long MainWindow::RowIndexById(std::wstring const &id) {
        if (!m_core || id.empty()) {
            return -1;
        }
        std::string want = to_utf8(hstring(id));
        size_t n = bridge_session_count(m_core);
        for (size_t i = 0; i < n; ++i) {
            char *json = bridge_session_json(m_core, i);
            std::string js = json ? json : "";
            bridge_string_free(json);
            if (staapjson::get_string(js, "id") == want) {
                return static_cast<long long>(i);
            }
        }
        return -1;
    }

    /* Show one empty-overlay state (macOS parity): title + detail +
     * one prominent button, or no button when there is nothing to
     * start. The terminal surface hides behind the overlay. */
    void MainWindow::ShowEmpty(std::wstring const &title,
                               std::wstring const &detail,
                               std::wstring const &button) {
        TermScroll().Visibility(Visibility::Collapsed);
        EmptyTitle().Text(hstring(title));
        EmptyDetail().Text(hstring(detail));
        if (button.empty()) {
            EmptyButton().Visibility(Visibility::Collapsed);
        } else {
            EmptyButton().Content(box_value(hstring(button)));
            EmptyButton().Visibility(Visibility::Visible);
        }
        EmptyPanel().Visibility(Visibility::Visible);
    }

    /* Show the selected run: the live terminal surface when its PTY is
     * live, else the empty overlay — the selected row's title with a
     * Restart/Spawn button, "Select a session" when nothing is picked,
     * or the "No sessions yet" CTA on an empty roster (all macOS
     * parity). Row/detail strings come from the core registry. */
    void MainWindow::ShowSelected() {
        if (m_pickerOpen) {
            return;
        }
        std::wstring wid = SelectedId();
        std::string id = wid.empty() ? "" : to_utf8(hstring{wid});
        if (!id.empty() && bridge_is_live(m_core, id.c_str())) {
            EmptyPanel().Visibility(Visibility::Collapsed);
            TermScroll().Visibility(Visibility::Visible);
            DisplayRow &row = m_rows[wid];
            const std::string &text =
                row.shown.empty() ? row.last_snapshot : row.shown;
            /* Catch a fresh attach with no cached text. */
            if (text.empty()) {
                char *snap = bridge_run_screen_text(m_core, id.c_str());
                std::string cur = snap ? snap : "";
                bridge_string_free(snap);
                row.last_snapshot = cur;
                row.shown = cur;
                TermBox().Text(to_hstring(cur));
            } else {
                TermBox().Text(to_hstring(text));
            }
            auto scroll = TermScroll();
            scroll.UpdateLayout();
            scroll.ChangeView(nullptr, scroll.ScrollableHeight(), nullptr);
            return;
        }
        long long at = RowIndexById(wid);
        if (at >= 0) {
            char *json = bridge_session_json(m_core, static_cast<size_t>(at));
            std::string js = json ? json : "";
            bridge_string_free(json);
            std::string title = staapjson::get_string(js, "title");
            std::string project = staapjson::get_string(js, "project");
            std::string harness = staapjson::get_string(js, "harness");
            if (title.empty()) {
                title = staapjson::get_string(js, "id");
            }
            std::string detail = project;
            if (!harness.empty()) {
                if (!detail.empty()) {
                    detail += " · ";
                }
                detail += harness;
            }
            /* Ended or historic row: restart/resume on the same id. */
            m_emptyMode = bridge_run_exited(m_core, id.c_str()) ? 1 : 2;
            ShowEmpty(to_wide(title), to_wide(detail), L"Restart / resume");
            return;
        }
        if (bridge_session_count(m_core) == 0) {
            m_emptyMode = 0;
            ShowEmpty(L"No sessions yet",
                      L"Spawned sessions appear here; history is restored on "
                      L"launch.",
                      L"New Session");
            return;
        }
        m_emptyMode = 2;
        ShowEmpty(L"Select a session", L"", L"");
    }

    /* Empty-overlay button: New Session on an empty roster, restart /
     * resume on an ended or historic row, nothing otherwise. */
    void MainWindow::EmptyButton_Click(IInspectable const &,
                                       RoutedEventArgs const &) {
        if (m_emptyMode == 0) {
            OpenNewSession();
        } else if (m_emptyMode == 1) {
            RestartSelected();
        }
    }

    /* Pump: the core registry feeds every live run (statuses, links,
     * re-sort inside via `bridge_pump_all` — the roster now actually
     * moves), then the selected run's delta reaches the output box, the
     * roster rebuilds on change, and persistence autosaves on a
     * throttle. */
    void MainWindow::OnTick(IInspectable const &, IInspectable const &) {
        static int save_tick = 0;
        bool dirty = bridge_pump_all(m_core) != 0;
        std::wstring wid = SelectedId();
        if (!wid.empty()) {
            std::string id = to_utf8(hstring{wid});
            if (bridge_is_live(m_core, id.c_str())) {
                DisplayRow &row = m_rows[wid];
                char *snap = bridge_run_screen_text(m_core, id.c_str());
                std::string cur = snap ? snap : "";
                bridge_string_free(snap);
                char *feed =
                    bridge_feed_delta(row.last_snapshot.c_str(), cur.c_str());
                if (feed) {
                    std::string chunk = feed;
                    free(feed);
                    /* Clear-screen prefix from the shared reconciler. */
                    constexpr const char *kClear = "\x1b[2J\x1b[H";
                    if (chunk.compare(0, strlen(kClear), kClear) == 0) {
                        row.shown = chunk.substr(strlen(kClear));
                        TermBox().Text(to_hstring(row.shown));
                    } else {
                        row.shown += chunk;
                        if (row.shown.size() > kShownCap) {
                            row.shown.erase(
                                0, row.shown.size() - kShownCap);
                        }
                        TermBox().Text(TermBox().Text() + to_hstring(chunk));
                    }
                    auto scroll = TermScroll();
                    scroll.UpdateLayout();
                    scroll.ChangeView(nullptr, scroll.ScrollableHeight(),
                                      nullptr);
                }
                row.last_snapshot = cur;
                dirty = true;
            }
        }
        RefreshRoster();
        /* Autosave throttle (replaces any manual Save): persist at most
         * every ~5s while dirty, like the shared pump persist. */
        if (dirty && (++save_tick % 100) == 0) {
            char *err = nullptr;
            if (bridge_core_save(m_core, &err) != 0) {
                free(err);
            }
        }
    }

    void MainWindow::OnClosed(IInspectable const &,
                              WindowEventArgs const &) {
        if (m_timer) {
            m_timer.Stop();
        }
        if (m_core) {
            /* Close hook persists, mirroring the GTK shell. */
            char *err = nullptr;
            if (bridge_core_save(m_core, &err) != 0) {
                free(err);
            }
        }
    }

    /* Single New Session entry: opens the in-place new-session screen on
     * the right side on every press (Ctrl+N and Ctrl+Shift+N route here
     * too). No repeat-last, no flyout menu. */
    void MainWindow::NewSessionButton_Click(IInspectable const &,
                                           RoutedEventArgs const &) {
        OpenNewSession();
    }

    /* Directory picker for the folder axis (issue #102): choosing in the
     * UI beats typing a path. Kept outside the handlers because XAML
     * event delegates take void handlers, not coroutines. */
    /* Restart/resume the selected run on the same id (parity with the
     * `r` key and the other shells): keeps title and links. */
    void MainWindow::RestartSelected() {
        if (!m_core) {
            return;
        }
        std::wstring wid = SelectedId();
        if (wid.empty()) {
            return;
        }
        std::string id = to_utf8(hstring{wid});
        char *err = nullptr;
        if (bridge_run_restart(m_core, id.c_str(), kCols, kRows, &err) != 0) {
            std::string msg = "Could not restart: ";
            msg += err ? err : "unknown error";
            SetStatus(to_hstring(msg));
            free(err);
            return;
        }
        m_fingerprint.clear();
        RefreshRoster();
        ShowSelected();
        SetStatus(L"Run restarted.");
    }

    /* Close (kill) the selected run + entry, then persist (parity with
     * the `x` key and the other shells). */
    void MainWindow::CloseSelected() {
        if (!m_core) {
            return;
        }
        std::wstring wid = SelectedId();
        if (wid.empty()) {
            return;
        }
        std::string id = to_utf8(hstring{wid});
        m_rows.erase(wid);
        bridge_run_close(m_core, id.c_str());
        char *err = nullptr;
        if (bridge_core_save(m_core, &err) != 0) {
            free(err);
        }
        m_fingerprint.clear();
        RefreshRoster();
        ShowSelected();
        SetStatus(L"Run closed.");
    }

    static fire_and_forget BrowseFolderAsync(TextBox folderBox, HWND hwnd) {
        if (!hwnd) {
            co_return;
        }
        winrt::Windows::Storage::Pickers::FolderPicker picker;
        picker.FileTypeFilter().Append(L"*");
        auto init = picker.as<IInitializeWithWindow>();
        winrt::check_hresult(init->Initialize(hwnd));
        if (auto picked = co_await picker.PickSingleFolderAsync()) {
            folderBox.Text(picked.Path());
        }
    }

    /* Currently selected picker CLI id (ComboBox Tag), or the first
     * available catalog id when nothing usable is selected. */
    std::string MainWindow::PickerCli() {
        int ci = PickerCliBox().SelectedIndex();
        if (ci >= 0 &&
            static_cast<size_t>(ci) < m_pickerClis.size() &&
            m_pickerClis[static_cast<size_t>(ci)].available) {
            return m_pickerClis[static_cast<size_t>(ci)].id;
        }
        for (auto const &cli : m_pickerClis) {
            if (cli.available) {
                return cli.id;
            }
        }
        return "muse";
    }

    /* Label "Default" with its resolved per-agent state (issue #102);
     * keeps the current tri-state pick. */
    void MainWindow::RefreshPickerYolo(std::string const &cli) {
        int sel = PickerYoloBox().SelectedIndex();
        std::string def_label = "Default (";
        def_label += bridge_yolo_default(m_core, cli.c_str()) ? "on" : "off";
        def_label += ")";
        m_pickerSync = true;
        PickerYoloBox().Items().SetAt(0, box_value(to_wide(def_label)));
        PickerYoloBox().SelectedIndex(sel);
        m_pickerSync = false;
    }

    /* Refresh the `runs: ...` preview line + Start enable from the
     * current picker state (core-owned preview copy; the host prefixes
     * the SSH chrome when remote). */
    void MainWindow::RefreshPickerPreview() {
        if (!m_pickerOpen) {
            return;
        }
        std::string cli = PickerCli();
        RefreshPickerYolo(cli);
        std::string folder = to_utf8(PickerFolderBox().Text());
        /* Core-owned yolo mapping (single copy of the tri-state rule). */
        int yolo = bridge_yolo_value(PickerYoloBox().SelectedIndex());
        char *text = bridge_spawn_preview(cli.c_str(), folder.c_str(), yolo);
        std::string line = text ? text : "";
        free(text);
        line = picker::with_ssh_prefix(m_machineHost, line);
        PickerPreview().Text(to_hstring(line));
        /* Spawn stays disabled while a missing CLI is selected: the
         * failure would be certain, so prevent it inline (fail visible
         * at the control, not after the click). */
        bool avail = false;
        int ci = PickerCliBox().SelectedIndex();
        if (ci >= 0 && static_cast<size_t>(ci) < m_pickerClis.size()) {
            avail = m_pickerClis[static_cast<size_t>(ci)].available;
        }
        PickerStartButton().IsEnabled(avail);
        PickerError().Text(L"");
    }

    /* Rebuild the CLI ComboBox from a fresh catalog (local catalog when
     * the host clears, remote catalog on probe arrival). The selected
     * CLI id survives when still listed and available, else the first
     * available CLI wins. */
    void MainWindow::RebuildCliRows(std::vector<picker::CliRow> fresh) {
        std::string keep;
        int ci = PickerCliBox().SelectedIndex();
        if (ci >= 0 && static_cast<size_t>(ci) < m_pickerClis.size() &&
            m_pickerClis[static_cast<size_t>(ci)].available) {
            keep = m_pickerClis[static_cast<size_t>(ci)].id;
        }
        m_pickerClis = std::move(fresh);
        m_pickerSync = true;
        PickerCliBox().Items().Clear();
        for (auto const &cli : m_pickerClis) {
            ComboBoxItem item;
            std::string label = cli.id;
            label += cli.available ? " — ready" : " — not installed";
            if (cli.available && !cli.path.empty()) {
                label += " (" + cli.path + ")";
            }
            item.Content(box_value(to_wide(label)));
            item.Tag(box_value(to_wide(cli.id)));
            item.IsEnabled(cli.available);
            if (!cli.available) {
                ToolTipService::SetToolTip(
                    item, box_value(winrt::hstring(
                              L"Install this CLI and ensure it is on PATH.")));
            }
            PickerCliBox().Items().Append(item);
        }
        bool picked = false;
        if (!keep.empty()) {
            for (uint32_t i = 0; i < PickerCliBox().Items().Size(); ++i) {
                auto item =
                    PickerCliBox().Items().GetAt(i).try_as<ComboBoxItem>();
                if (item && item.IsEnabled() &&
                    to_utf8(unbox_value<hstring>(item.Tag())) == keep) {
                    PickerCliBox().SelectedIndex(static_cast<int32_t>(i));
                    picked = true;
                    break;
                }
            }
        }
        if (!picked) {
            for (uint32_t i = 0; i < PickerCliBox().Items().Size(); ++i) {
                auto item =
                    PickerCliBox().Items().GetAt(i).try_as<ComboBoxItem>();
                if (item && item.IsEnabled()) {
                    PickerCliBox().SelectedIndex(static_cast<int32_t>(i));
                    picked = true;
                    break;
                }
            }
        }
        if (!picked) {
            PickerCliBox().SelectedIndex(-1);
        }
        m_pickerSync = false;
    }

    /* Rebuild the machine dropdown (This machine + saved hosts + the
     * Add row). `select_host` names the host to preselect, if saved. */
    void MainWindow::RebuildMachineBox(std::string const &select_host) {
        char *hosts_json = bridge_saved_hosts_json(m_core);
        auto saved =
            picker::parse_saved_hosts(hosts_json ? hosts_json : "[]");
        bridge_string_free(hosts_json);
        m_machineHosts.clear();
        m_pickerSync = true;
        MachineBox().Items().Clear();
        MachineBox().Items().Append(
            box_value(winrt::hstring(L"This machine")));
        m_machineHosts.emplace_back();
        size_t sel = 0;
        for (auto const &row : saved) {
            MachineBox().Items().Append(
                box_value(to_wide(row.name + " — " + row.host)));
            m_machineHosts.push_back(row.host);
            if (!select_host.empty() && select_host == row.host) {
                sel = m_machineHosts.size() - 1;
            }
        }
        MachineBox().Items().Append(
            box_value(winrt::hstring(L"Add remote machine…")));
        m_machineHosts.emplace_back();
        m_machineSel = sel;
        MachineBox().SelectedIndex(static_cast<int32_t>(sel));
        m_pickerSync = false;
        m_machineHost =
            (sel > 0 && sel < m_machineHosts.size() &&
             !m_machineHosts[sel].empty())
                ? m_machineHosts[sel]
                : std::string{};
        SyncMachineButtons();
    }

    /* Reload the recents dropdown: per-host persisted recents when
     * remote (core-owned, never network), local MRU otherwise. The
     * entry keeps whatever the user typed. */
    void MainWindow::ReloadPickerRecents() {
        char *raw = m_machineHost.empty()
                        ? bridge_recent_json(m_core)
                        : bridge_remote_recent_json(m_core,
                                                    m_machineHost.c_str());
        std::string js = raw ? raw : "[]";
        bridge_string_free(raw);
        m_pickerRecents = picker::parse_recents(js);
        m_pickerSync = true;
        PickerRecentBox().Items().Clear();
        PickerRecentBox().Items().Append(
            box_value(winrt::hstring(L"Type a folder…")));
        for (auto const &r : m_pickerRecents) {
            PickerRecentBox().Items().Append(box_value(to_wide(r)));
        }
        PickerRecentBox().SelectedIndex(0);
        m_pickerSync = false;
    }

    /* Refresh the CLI catalog for the machine axis (blank = this
     * machine). Remote hosts probe in the background with stale-drop;
     * returning to blank restores the synchronous local catalog. */
    void MainWindow::KickProbe() {
        unsigned my = ++m_probeGen;
        if (m_machineHost.empty()) {
            MachineStatus().Text(L"");
            char *raw = bridge_clis_json();
            std::string js = raw ? raw : "[]";
            bridge_string_free(raw);
            RebuildCliRows(picker::parse_clis(js));
            RefreshPickerPreview();
            return;
        }
        std::string host = m_machineHost;
        MachineStatus().Text(
            winrt::hstring(to_wide("Probing " + host + "…")));
        /* NB: the type needs its namespace: the inherited
         * DependencyObject::DispatcherQueue() method hides the bare
         * type name inside this member function. */
        Microsoft::UI::Dispatching::DispatcherQueue dq = DispatcherQueue();
        auto strong = get_strong();
        std::thread([strong, my, host, dq]() {
            /* One bounded ssh round trip, off the UI thread. */
            char *raw = bridge_clis_remote(host.c_str());
            std::string js = raw ? raw : "[]";
            bridge_string_free(raw);
            dq.TryEnqueue([strong, my, host, js]() {
                /* Stale arrival or closed screen: drop. */
                if (!strong->m_pickerOpen || my != strong->m_probeGen) {
                    return;
                }
                strong->RebuildCliRows(picker::parse_clis(js));
                size_t ready = 0;
                for (auto const &cli : strong->m_pickerClis) {
                    if (cli.available) {
                        ++ready;
                    }
                }
                strong->MachineStatus().Text(winrt::hstring(to_wide(
                    "Probed " + host + ": " + std::to_string(ready) +
                    " CLI(s) available.")));
                strong->RefreshPickerPreview();
            });
        }).detach();
    }

    /* Refresh everything the machine axis owns: folder default inherit,
     * recents, remote catalog probe, browser listing. */
    void MainWindow::AfterMachine() {
        if (!m_machineHost.empty()) {
            /* A saved host default fills a blank folder entry (never
             * clobbers what the user typed). */
            std::string cur =
                picker::trim_host(to_utf8(PickerFolderBox().Text()));
            if (cur.empty()) {
                char *dflt = bridge_remote_default_folder(
                    m_core, m_machineHost.c_str());
                if (dflt && *dflt) {
                    m_pickerSync = true;
                    PickerFolderBox().Text(to_wide(dflt));
                    m_pickerSync = false;
                }
                bridge_string_free(dflt);
            }
        }
        PickerFolderBox().PlaceholderText(
            m_machineHost.empty()
                ? winrt::hstring(L"Blank = current folder")
                : winrt::hstring(L"Blank = remote login folder"));
        /* The local Browse picker cannot choose remote folders. */
        PickerBrowseButton().IsEnabled(m_machineHost.empty());
        ReloadPickerRecents();
        KickProbe();
        BrowseRefresh();
        RefreshPickerPreview();
    }

    /* Fill the browser rows from a listing payload. Directories carry
     * their full path in the Tag for activate-to-fill; files are
     * disabled (inert). */
    void MainWindow::RenderBrowser(std::string const &json) {
        auto entries = picker::parse_remote_list(json);
        m_pickerSync = true;
        RemoteBrowserList().Items().Clear();
        for (auto const &row : entries) {
            ListViewItem item;
            std::string label = row.is_dir ? row.name + "/" : row.name;
            item.Content(box_value(to_wide(label)));
            item.Tag(box_value(
                to_wide(picker::remote_join(m_browsePath, row.name))));
            item.IsEnabled(row.is_dir);
            RemoteBrowserList().Items().Append(item);
        }
        m_pickerSync = false;
        if (entries.empty()) {
            BrowserStatus().Text(
                L"Empty or unreachable - check the path, or Test the host.");
        } else {
            BrowserStatus().Text(winrt::hstring(to_wide(
                std::to_string(entries.size()) +
                " entries - open a folder to fill the entry.")));
        }
    }

    /* List the entry path (when non-blank), else the last listed dir,
     * else the host default. Manual only — never per keystroke. The
     * entry is the single source the Start action reads. */
    void MainWindow::BrowseRefresh() {
        ++m_browseGen;
        if (m_machineHost.empty()) {
            BrowserWrap().Visibility(Visibility::Collapsed);
            BrowserStatus().Text(L"");
            return;
        }
        BrowserWrap().Visibility(Visibility::Visible);
        std::string entry =
            picker::trim_host(to_utf8(PickerFolderBox().Text()));
        std::string path = entry;
        if (path.empty()) {
            if (!m_browsePath.empty()) {
                path = m_browsePath;
            } else {
                char *dflt = bridge_remote_default_folder(
                    m_core, m_machineHost.c_str());
                if (dflt && *dflt) {
                    path = dflt;
                }
                bridge_string_free(dflt);
            }
        }
        if (path.empty()) {
            BrowserStatus().Text(
                L"No default folder - type a path above, then press Refresh.");
            RenderBrowser("[]");
            return;
        }
        unsigned my = m_browseGen;
        std::string host = m_machineHost;
        BrowserStatus().Text(
            winrt::hstring(to_wide("Listing " + path + " …")));
        Microsoft::UI::Dispatching::DispatcherQueue dq = DispatcherQueue();
        auto strong = get_strong();
        std::thread([strong, my, host, path, dq]() {
            /* One bounded ssh round trip, off the UI thread. */
            char *raw = bridge_remote_list_json(host.c_str(), path.c_str());
            std::string js = raw ? raw : "[]";
            bridge_string_free(raw);
            dq.TryEnqueue([strong, my, path, js]() {
                if (!strong->m_pickerOpen || my != strong->m_browseGen) {
                    return;
                }
                strong->m_browsePath = path;
                strong->RemotePathLabel().Text(to_hstring(path));
                strong->RenderBrowser(js);
            });
        }).detach();
    }

    void MainWindow::MachineBox_SelectionChanged(
        IInspectable const &, SelectionChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        int idx = MachineBox().SelectedIndex();
        if (idx >= 0 &&
            static_cast<size_t>(idx) + 1 == m_machineHosts.size()) {
            /* Add row: revert to the previous pick, open the SSH add
             * dialog. */
            m_pickerSync = true;
            MachineBox().SelectedIndex(static_cast<int32_t>(m_machineSel));
            m_pickerSync = false;
            ShowAddMachineDialog({});
            return;
        }
        if (idx < 0 || static_cast<size_t>(idx) >= m_machineHosts.size()) {
            return;
        }
        m_machineSel = static_cast<size_t>(idx);
        m_machineHost = m_machineHosts[m_machineSel];
        AfterMachine();
        SyncMachineButtons();
    }

    /* Edit/Remove sensitivity follows the selection: only a saved host
     * row (a non-empty machine host) can be edited or removed. This
     * machine and the Add row leave both buttons disabled - the keyboard
     * path (Delete on the focused dropdown, Tab-reached buttons) matches
     * the mouse path. */
    void MainWindow::SyncMachineButtons() {
        bool saved = m_pickerOpen && !m_machineHost.empty();
        MachineEditButton().IsEnabled(saved);
        MachineRemoveButton().IsEnabled(saved);
    }

    /* Remove the selected saved host (Remove button / Delete on the
     * focused dropdown): the core forgets the host and its folder
     * memory, the change autosaves, and the picker falls back to This
     * machine instead of pointing at a deleted host. Refusals land in
     * the picker error line (fail visible, never a silent no-op). */
    void MainWindow::RemoveSavedMachine() {
        if (!m_pickerOpen || !m_core || m_machineHost.empty()) {
            return;
        }
        char *err = nullptr;
        if (bridge_saved_host_remove(m_core, m_machineHost.c_str(),
                                     &err) != 0) {
            std::string msg = err ? err : "Could not remove.";
            free(err);
            PickerError().Text(to_hstring(msg));
            return;
        }
        char *serr = nullptr;
        if (bridge_core_save(m_core, &serr) != 0) {
            free(serr);
        }
        PickerError().Text(L"");
        RebuildMachineBox({});
        AfterMachine();
    }

    void MainWindow::MachineEdit_Click(
        IInspectable const &, RoutedEventArgs const &) {
        if (m_pickerOpen && !m_machineHost.empty()) {
            ShowAddMachineDialog(m_machineHost);
        }
    }

    void MainWindow::MachineRemove_Click(
        IInspectable const &, RoutedEventArgs const &) {
        RemoveSavedMachine();
    }

    void MainWindow::MachineBox_KeyDown(
        IInspectable const &, KeyRoutedEventArgs const &args) {
        /* Delete on the focused dropdown removes the selected saved
         * host (same funnel as the Remove button); anything else keeps
         * the default ComboBox behavior. */
        if (args.Key() == Windows::System::VirtualKey::Delete &&
            !m_machineHost.empty()) {
            RemoveSavedMachine();
            args.Handled(true);
        }
    }

    void MainWindow::PickerFolderBox_TextChanged(
        IInspectable const &, TextChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        RefreshPickerPreview();
    }

    void MainWindow::PickerRecentBox_SelectionChanged(
        IInspectable const &, SelectionChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        /* Index 0 is the "type a folder" placeholder. */
        int ri = PickerRecentBox().SelectedIndex();
        if (ri > 0 &&
            static_cast<size_t>(ri - 1) < m_pickerRecents.size()) {
            m_pickerSync = true;
            PickerFolderBox().Text(
                to_wide(m_pickerRecents[static_cast<size_t>(ri - 1)]));
            m_pickerSync = false;
        }
        RefreshPickerPreview();
    }

    void MainWindow::PickerCliBox_SelectionChanged(
        IInspectable const &, SelectionChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        RefreshPickerPreview();
    }

    void MainWindow::PickerYoloBox_SelectionChanged(
        IInspectable const &, SelectionChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        RefreshPickerPreview();
    }

    void MainWindow::RemoteBrowser_SelectionChanged(
        IInspectable const &, SelectionChangedEventArgs const &) {
        if (!m_pickerOpen || m_pickerSync) {
            return;
        }
        auto item =
            RemoteBrowserList().SelectedItem().try_as<ListViewItem>();
        if (!item || !item.IsEnabled()) {
            return;
        }
        /* Files are disabled (inert); a directory fills the folder
         * entry and relists. */
        std::string full = to_utf8(unbox_value<hstring>(item.Tag()));
        if (full.empty()) {
            return;
        }
        m_pickerSync = true;
        PickerFolderBox().Text(to_wide(full));
        RemoteBrowserList().SelectedIndex(-1);
        m_pickerSync = false;
        BrowseRefresh();
        RefreshPickerPreview();
    }

    void MainWindow::BrowserUp_Click(IInspectable const &,
                                    RoutedEventArgs const &) {
        if (!m_pickerOpen) {
            return;
        }
        std::string cur =
            picker::trim_host(to_utf8(PickerFolderBox().Text()));
        if (cur.empty()) {
            cur = m_browsePath;
        }
        std::string parent = picker::remote_parent(cur);
        if (parent.empty()) {
            return;
        }
        m_pickerSync = true;
        PickerFolderBox().Text(to_wide(parent));
        m_pickerSync = false;
        BrowseRefresh();
        RefreshPickerPreview();
    }

    void MainWindow::BrowserRefresh_Click(IInspectable const &,
                                         RoutedEventArgs const &) {
        if (!m_pickerOpen) {
            return;
        }
        BrowseRefresh();
        RefreshPickerPreview();
    }

    void MainWindow::PickerBrowse_Click(IInspectable const &,
                                       RoutedEventArgs const &) {
        // Owner window for the folder picker (issue #102).
        HWND hwnd{nullptr};
        if (auto native = this->try_as<IWindowNative>()) {
            native->get_WindowHandle(&hwnd);
        }
        BrowseFolderAsync(PickerFolderBox(), hwnd);
    }

    /* Spawn the confirmed combination: folder x CLI + one-shot yolo,
     * local or over SSH. A missing local folder refuses inline (the
     * screen stays open for a fix); remote paths skip that gate (they
     * cannot be stated locally) and let the core report failures. A
     * spawn failure closes the screen and names the fix in the status
     * line, like every other shell. */
    void MainWindow::PickerStart_Click(IInspectable const &,
                                      RoutedEventArgs const &) {
        if (!m_pickerOpen) {
            return;
        }
        std::string host = m_machineHost;
        std::string folder =
            picker::trim_host(to_utf8(PickerFolderBox().Text()));
        if (host.empty() && !folder.empty()) {
            DWORD attrs = GetFileAttributesA(folder.c_str());
            if (attrs == INVALID_FILE_ATTRIBUTES ||
                !(attrs & FILE_ATTRIBUTE_DIRECTORY)) {
                PickerError().Text(
                    winrt::hstring(to_wide("No such folder: " + folder)));
                return;
            }
        }
        std::string cli = PickerCli();
        /* Core-owned yolo mapping; the registry spawn attaches a real
         * roster row under the shared cap (a refusal names per-run
         * close, like every other shell). */
        int yolo = bridge_yolo_value(PickerYoloBox().SelectedIndex());
        char id[256] = {0};
        char *err = nullptr;
        int rc;
        if (!host.empty()) {
            /* Remote: the local PTY runs `ssh -t <host> <cli...>` under
             * the shared cap, so interactive auth prompts surface in the
             * terminal; a blank folder means the remote login
             * directory. */
            rc = bridge_run_spawn_remote(
                m_core, host.c_str(),
                folder.empty() ? nullptr : folder.c_str(), cli.c_str(),
                yolo, kCols, kRows, id, sizeof id, &err);
        } else {
            rc = bridge_run_spawn(m_core, cli.c_str(),
                                  folder.empty() ? nullptr : folder.c_str(),
                                  yolo, kCols, kRows, id, sizeof id, &err);
        }
        if (rc != 0) {
            std::string msg = err ? err : "unknown error";
            free(err);
            if (msg.find("live runs") != std::string::npos) {
                PickerError().Text(to_hstring(msg));
                return;
            }
            SetStatus(to_hstring("Could not spawn: " + msg));
            CloseNewSession();
            return;
        }
        char *done = bridge_spawn_preview(cli.c_str(), folder.c_str(), yolo);
        std::string line = done ? done : "";
        free(done);
        if (!host.empty()) {
            line = picker::with_ssh_prefix(host, line);
            /* Remember the host for next time (shell-local only). */
            try {
                Windows::Storage::ApplicationData::Current()
                    .LocalSettings()
                    .Values()
                    .Insert(kRemoteHostKey, box_value(to_wide(host)));
            } catch (...) {
            }
        }
        SetStatus(winrt::hstring(to_wide(line)));
        CloseNewSession();
        /* Hand the keyboard to the new session (takes the keyboard on
         * spawn, like the macOS shell's `n` key): without this, focus
         * stays on the Start button, where Return re-clicks instead of
         * submitting the typed prompt. */
        TermBox().Focus(Microsoft::UI::Xaml::FocusState::Programmatic);
    }

    void MainWindow::PickerCancel_Click(IInspectable const &,
                                       RoutedEventArgs const &) {
        CloseNewSession();
    }

    /* Hide the in-place screen and return to the terminal. In-flight
     * probe/listing arrivals land stale and drop. */
    void MainWindow::CloseNewSession() {
        if (!m_pickerOpen) {
            return;
        }
        m_pickerOpen = false;
        ++m_browseGen;
        ++m_probeGen;
        NewSessionScroll().Visibility(Visibility::Collapsed);
        ShowSelected();
    }

    /* Open the in-place new-session screen on the right side: folder
     * entry (+ Browse for this machine, remote browser for saved
     * hosts) + recents, machine dropdown (This machine / saved hosts /
     * Add row), CLI dropdown, yolo tri-state, and the live `runs: ...`
     * preview. Catalog, recents, and saved hosts re-read on every open
     * so the list is never stale. Shown every time New session is
     * pressed. */
    void MainWindow::OpenNewSession() {
        if (!m_core) {
            return;
        }
        if (m_pickerOpen) {
            CloseNewSession();
        }
        char *clis_raw = bridge_clis_json();
        std::string clis_json = clis_raw ? clis_raw : "[]";
        bridge_string_free(clis_raw);
        if (picker::parse_clis(clis_json).empty()) {
            SetStatus(L"No agent CLI catalog: cannot open the picker.");
            return;
        }
        char *recents_raw = bridge_recent_json(m_core);
        std::string recents_json = recents_raw ? recents_raw : "[]";
        bridge_string_free(recents_raw);
        m_pickerRecents = picker::parse_recents(recents_json);
        /* Shell-local last SSH host (LocalSettings; the C ABI exposes
         * no last-host getter). */
        std::string last_host;
        try {
            auto values = Windows::Storage::ApplicationData::Current()
                              .LocalSettings()
                              .Values();
            if (values.HasKey(kRemoteHostKey)) {
                last_host = to_utf8(
                    unbox_value<hstring>(values.Lookup(kRemoteHostKey)));
            }
        } catch (...) {
        }
        /* Yolo tri-state (Default / On once / Off once), safe by
         * default; the preview line names the exact combination. */
        m_pickerSync = true;
        PickerYoloBox().Items().Clear();
        PickerYoloBox().Items().Append(
            box_value(winrt::hstring(L"Default")));
        PickerYoloBox().Items().Append(
            box_value(winrt::hstring(L"On (once)")));
        PickerYoloBox().Items().Append(
            box_value(winrt::hstring(L"Off (once)")));
        PickerYoloBox().SelectedIndex(0);
        ToolTipService::SetToolTip(
            PickerYoloBox(), box_value(winrt::hstring(
                                  L"Yolo lets the agent run commands without asking. "
                                  L"Default follows the per-agent config; once-choices "
                                  L"apply to this run only and are never saved.")));
        /* Folder entry starts from the local MRU head (a saved host
         * default fills a blank entry on machine change instead). */
        PickerFolderBox().Text(
            to_wide(m_pickerRecents.empty() ? "" : m_pickerRecents[0]));
        PickerError().Text(L"");
        m_pickerSync = false;
        m_browsePath.clear();
        m_pickerOpen = true;
        EmptyPanel().Visibility(Visibility::Collapsed);
        TermScroll().Visibility(Visibility::Collapsed);
        NewSessionScroll().Visibility(Visibility::Visible);
        /* Fresh catalog + machine state (a remote probe may replace the
         * CLI rows when a host is preselected). */
        m_pickerSync = true;
        PickerCliBox().SelectedIndex(-1);
        m_pickerSync = false;
        RebuildCliRows(picker::parse_clis(clis_json));
        RebuildMachineBox(last_host);
        AfterMachine();
        PickerFolderBox().Focus(Microsoft::UI::Xaml::FocusState::Programmatic);
    }

    /* SSH machine dialog (opened from the machine dropdown's Add row, or
     * from Edit for one saved host with `edit_host` set):
     * display name, user@hostname-or-alias host, default folder,
     * Test/Save/Cancel. Save stores a saved host via the core (edit mode
     * renames in place: the typo fix - saved order kept, the cap never
     * bites, folder memory follows); Test
     * runs the core tester off-thread and reports reachable plus CLIs,
     * or the core reason line on failure. */
    fire_and_forget MainWindow::ShowAddMachineDialog(std::string edit_host) {
        auto lifetime = get_strong();
        if (!m_core || !m_pickerOpen) {
            co_return;
        }
        auto caption = [](Style const &s, const wchar_t *t) {
            TextBlock h;
            h.Text(t);
            h.Style(s);
            return h;
        };
        Style cap = Application().Current()
                        .Resources()
                        .Lookup(box_value(L"CaptionTextBlockStyle"))
                        .as<Style>();
        TextBox nameBox;
        nameBox.PlaceholderText(L"e.g. Build box");
        nameBox.MinWidth(260);
        TextBox hostBox;
        hostBox.PlaceholderText(
            to_hstring(picker::machine_placeholder_hint()));
        hostBox.MinWidth(260);
        TextBox folderBox;
        folderBox.PlaceholderText(L"Blank = remote home directory");
        folderBox.MinWidth(260);
        if (!edit_host.empty()) {
            /* Edit mode: prefill from the saved row; the original host
             * stays in edit_host so saving renames it, never duplicates. */
            char *hosts_json = bridge_saved_hosts_json(m_core);
            auto saved =
                picker::parse_saved_hosts(hosts_json ? hosts_json : "[]");
            bridge_string_free(hosts_json);
            for (auto const &row : saved) {
                if (row.host == edit_host) {
                    nameBox.Text(to_hstring(row.name));
                    hostBox.Text(to_hstring(row.host));
                    if (!row.default_folder.empty()) {
                        folderBox.Text(to_hstring(row.default_folder));
                    }
                    break;
                }
            }
        }
        TextBlock result;
        result.Style(cap);
        result.TextWrapping(TextWrapping::Wrap);
        TextBlock error;
        error.Style(cap);
        error.TextWrapping(TextWrapping::Wrap);
        StackPanel panel;
        panel.Spacing(8);
        panel.Children().Append(caption(cap, L"Display name"));
        panel.Children().Append(nameBox);
        panel.Children().Append(caption(cap, L"Host"));
        panel.Children().Append(hostBox);
        panel.Children().Append(caption(cap, L"Default folder (optional)"));
        panel.Children().Append(folderBox);
        panel.Children().Append(result);
        panel.Children().Append(error);
        struct AddState {
            unsigned testGen = 0;
            bool closed = false;
        };
        auto state = std::make_shared<AddState>();
        ContentDialog dialog;
        dialog.Title(box_value(winrt::hstring(
            edit_host.empty() ? L"Add remote machine" : L"Edit remote machine")));
        dialog.Content(panel);
        dialog.CloseButtonText(L"Cancel");
        dialog.DefaultButton(ContentDialogButton::Close);
        dialog.XamlRoot(this->Content().XamlRoot());
        dialog.Closed([state](ContentDialog const &,
                             ContentDialogClosedEventArgs const &) {
            state->closed = true;
            ++state->testGen;
        });
        StackPanel actions;
        actions.Orientation(Orientation::Horizontal);
        actions.Spacing(8);
        actions.HorizontalAlignment(HorizontalAlignment::Right);
        Button test;
        test.Content(box_value(winrt::hstring(L"Test")));
        ToolTipService::SetToolTip(
            test, box_value(winrt::hstring(L"Check SSH reachability now")));
        /* Test must not auto-close: panel button, not a dialog
         * button. Runs the core tester off the UI thread. */
        test.Click([this, state, hostBox, result](IInspectable const &,
                                                 RoutedEventArgs const &) {
            ++state->testGen;
            unsigned my = state->testGen;
            std::string host =
                picker::trim_host(to_utf8(hostBox.Text()));
            if (host.empty()) {
                result.Text(L"Enter a host to test.");
                return;
            }
            result.Text(winrt::hstring(to_wide("Testing " + host + " …")));
            Microsoft::UI::Dispatching::DispatcherQueue dq =
                DispatcherQueue();
            auto strong = get_strong();
            std::thread([strong, state, my, host, dq, result]() {
                char *raw = bridge_remote_test_json(host.c_str());
                std::string js = raw ? raw : "";
                bridge_string_free(raw);
                dq.TryEnqueue([strong, state, my, host, js, result]() {
                    if (state->closed || my != state->testGen) {
                        return;
                    }
                    bool reachable =
                        js.find("\"reachable\":true") != std::string::npos;
                    if (reachable) {
                        /* The test payload reuses the catalog shape,
                         * so the catalog parse reads it directly. */
                        auto clis = picker::parse_clis(js);
                        std::string have;
                        size_t nhave = 0;
                        for (auto const &cli : clis) {
                            if (cli.available) {
                                have += (nhave ? ", " : "") + cli.id;
                                ++nhave;
                            }
                        }
                        result.Text(
                            nhave
                                ? winrt::hstring(to_wide("Reachable - CLIs: " +
                                                        have + "."))
                                : winrt::hstring(
                                      L"Reachable - no known CLIs found."));
                    } else {
                        /* Failure copy renders the core message verbatim
                         * (dumb-shell contract: no per-shell error parsing);
                         * the host stays in parens as before for context. */
                        std::string reason = picker::test_message(js);
                        std::string line =
                            reason.empty()
                                ? "Unreachable - check the host name and SSH access (" +
                                      host + ")."
                                : reason + " (" + host + ").";
                        result.Text(winrt::hstring(to_wide(line)));
                    }
                });
            }).detach();
        });
        Button cancel;
        cancel.Content(box_value(winrt::hstring(L"Cancel")));
        cancel.Click(
            [dialog](IInspectable const &, RoutedEventArgs const &) mutable {
                dialog.Hide();
            });
        Button save;
        save.Content(box_value(winrt::hstring(L"Save")));
        save.Style(Application().Current()
                       .Resources()
                       .Lookup(box_value(L"AccentButtonStyle"))
                       .as<Style>());
        save.Click([this, state, edit_host, nameBox, hostBox, folderBox, error,
                    dialog](IInspectable const &,
                            RoutedEventArgs const &) mutable {
            std::string name =
                picker::trim_host(to_utf8(nameBox.Text()));
            std::string host =
                picker::trim_host(to_utf8(hostBox.Text()));
            std::string folder =
                picker::trim_host(to_utf8(folderBox.Text()));
            if (name.empty()) {
                error.Text(L"Give the machine a display name.");
                return;
            }
            if (host.empty()) {
                error.Text(
                    L"Enter a host: user@hostname or an ssh-config alias.");
                return;
            }
            /* Add mode stores; edit mode renames in place (saved order
             * kept, the cap never bites, folder memory follows). */
            const char *folder_arg =
                folder.empty() ? nullptr : folder.c_str();
            char *err = nullptr;
            int add_rc = edit_host.empty()
                             ? bridge_saved_host_add(m_core, name.c_str(),
                                                     host.c_str(), folder_arg,
                                                     &err)
                             : bridge_saved_host_rename(m_core,
                                                        edit_host.c_str(),
                                                        name.c_str(), host.c_str(),
                                                        folder_arg, &err);
            if (add_rc != 0) {
                std::string msg = err ? err : "Could not save.";
                free(err);
                error.Text(to_hstring(msg));
                return;
            }
            char *serr = nullptr;
            if (bridge_core_save(m_core, &serr) != 0) {
                free(serr);
            }
            state->closed = true;
            ++state->testGen;
            dialog.Hide();
            /* The closed handler already marked the test stale; select
             * the saved host (fires the machine change path). */
            RebuildMachineBox(host);
            AfterMachine();
        });
        actions.Children().Append(test);
        actions.Children().Append(cancel);
        actions.Children().Append(save);
        panel.Children().Append(actions);
        co_await dialog.ShowAsync();
    }






    void MainWindow::FilterBox_TextChanged(
        IInspectable const &, TextChangedEventArgs const &) {
        m_filter = to_utf8(FilterBox().Text());
        /* Core-owned filter text (snaps selection); the lists read the
         * same core state. */
        bridge_set_filter(m_core, m_filter.c_str());
        m_fingerprint.clear(); /* Force a roster rebuild on next tick. */
        RefreshRoster();
    }

    /* One shared selection across the four group lists: a pick in
     * any list clears the other three and shows that run.
     * Null-selection events (list clears during a rebuild) are
     * ignored so the core selection survives the repopulate; m_syncing
     * covers programmatic moves. */
    void MainWindow::Roster_SelectionChanged(
        IInspectable const &sender, SelectionChangedEventArgs const &) {
        if (m_syncing) {
            return;
        }
        auto picked = sender.try_as<ListView>();
        if (!picked) {
            return;
        }
        auto item = picked.SelectedItem().try_as<ListViewItem>();
        if (!item) {
            return;
        }
        std::wstring id = std::wstring(unbox_value<hstring>(item.Tag()));
        m_syncing = true;
        ListView lists[] = {NeedsInputList(), IdleList(), WorkingList(),
                            HistoryList()};
        for (auto const &list : lists) {
            if (list != picked) {
                list.SelectedIndex(-1);
            }
        }
        m_syncing = false;
        /* Write through to the core selection (SelectRowById mirrors). */
        SelectRowById(id);
    }

    /* History expander toggle: core-owned expansion + persist (same
     * funnel every shell shares). Programmatic sync in RefreshRoster
     * runs under m_syncing so it never writes back; the fingerprint
     * carries expansion so the next tick syncs without rebuilding. */
    void MainWindow::HistoryExpander_Expanding(
        IInspectable const &,
        Microsoft::UI::Xaml::Controls::ExpanderExpandingEventArgs const &) {
        WriteHistoryExpanded(true);
    }
    void MainWindow::HistoryExpander_Collapsed(
        IInspectable const &,
        Microsoft::UI::Xaml::Controls::ExpanderCollapsedEventArgs const &) {
        WriteHistoryExpanded(false);
    }
    void MainWindow::WriteHistoryExpanded(bool expanded) {
        if (m_syncing || !m_core) {
            return;
        }
        bridge_set_history_expanded(m_core, expanded ? 1 : 0);
        char *err = nullptr;
        if (bridge_core_save(m_core, &err) != 0) {
            free(err);
        }
        m_fingerprint.clear();
    }

    /* Current sidebar width in pixels; 0 when the column is star/auto
     * (never expected — the XAML pins pixels — but guarded anyway). */
    double MainWindow::SidebarWidthPx() {
        return SidebarColumn().ActualWidth();
    }

    /* Clamp + apply + persist one sidebar width. The range is the
     * shared core rule (`staap_clamp_sidebar`: 220..480px); persistence
     * rides LocalSettings (local-only trust: plain local store). */
    void MainWindow::SetSidebarWidth(double w) {
        double clamped = bridge_clamp_sidebar(w);
        SidebarColumn().Width(GridLengthHelper::FromPixels(clamped));
        try {
            Windows::Storage::ApplicationData::Current()
                .LocalSettings()
                .Values()
                .Insert(kSidebarWidthKey, box_value(clamped));
        } catch (...) {
        }
    }

    /* Thumb drag: HorizontalChange is already in DIPs along the drag
     * axis, so it adds straight onto the column width. (Unlike
     * KeyRoutedEventArgs, DragDeltaEventArgs carries no Handled flag
     * — there is nothing to mark: the event has no routing to stop.) */
    void MainWindow::SidebarThumb_DragDelta(
        IInspectable const &,
        Controls::Primitives::DragDeltaEventArgs const &args) {
        SetSidebarWidth(SidebarWidthPx() + args.HorizontalChange());
    }

    /* Keyboard parity for the grip (fail-visible + non-color cue: the
     * Thumb template's grip lights up on focus/hover/press, and the
     * thumb exposes an automation name + tooltip): Left/Right nudge
     * in 8px steps, Home/End jump to min/max. Up/Down mirror
     * Left/Right for screen-reader arrow conventions; anything else
     * stays with the shell's global KeyDown handler. */
    void MainWindow::SidebarThumb_KeyDown(
        IInspectable const &, KeyRoutedEventArgs const &args) {
        double w = SidebarWidthPx();
        switch (args.Key()) {
        case Windows::System::VirtualKey::Left:
        case Windows::System::VirtualKey::Up:
            SetSidebarWidth(w - kSidebarKeyStep);
            args.Handled(true);
            return;
        case Windows::System::VirtualKey::Right:
        case Windows::System::VirtualKey::Down:
            SetSidebarWidth(w + kSidebarKeyStep);
            args.Handled(true);
            return;
        case Windows::System::VirtualKey::Home:
            SetSidebarWidth(kSidebarMin);
            args.Handled(true);
            return;
        case Windows::System::VirtualKey::End:
            SetSidebarWidth(kSidebarMax);
            args.Handled(true);
            return;
        default:
            return;
        }
    }

    /* Map a WinUI virtual key to the core's logical key name (the
     * shared `staap_key_encode` table owns the bytes; this only
     * translates). Returns "" when the key has no logical name. */
    static std::string vk_name(int vk) {
        switch (vk) {
        case 0x0D: return "enter";
        case 0x08: return "backspace";
        case 0x09: return "tab";
        case 0x1B: return "escape";
        case 0x26: return "up";
        case 0x28: return "down";
        case 0x27: return "right";
        case 0x25: return "left";
        case 0x24: return "home";
        case 0x23: return "end";
        case 0x2D: return "insert";
        case 0x2E: return "delete";
        case 0x21: return "pageup";
        case 0x22: return "pagedown";
        case 0x70: return "f1";
        case 0x71: return "f2";
        case 0x72: return "f3";
        case 0x73: return "f4";
        case 0x74: return "f5";
        case 0x75: return "f6";
        case 0x76: return "f7";
        case 0x77: return "f8";
        case 0x78: return "f9";
        case 0x79: return "f10";
        case 0x7A: return "f11";
        case 0x7B: return "f12";
        default: return {};
        }
    }

    /* Encode via the shared core key table and forward. Returns true
     * when handled (the native control must not also process it). */
    bool MainWindow::EncodeForward(int vk, char32_t text, bool ctrl,
                                   bool shift, bool alt) {
        /* Reserve rule (shared): Ctrl+Shift+C/V stay with the native
         * control for copy/paste. */
        if (ctrl && shift && (vk == 'C' || vk == 'V')) {
            return false;
        }
        std::string name = vk_name(vk);
        /* Printable char without a logical name: the key name is the
         * char itself (the core table prefers the typed char). */
        std::string text_utf8;
        if (text != 0 && text <= 0x10FFFF) {
            char tmp[5]{};
            if (text < 0x80) {
                tmp[0] = static_cast<char>(text);
                text_utf8 = tmp;
            } else if (text < 0x800) {
                tmp[0] = static_cast<char>(0xC0 | (text >> 6));
                tmp[1] = static_cast<char>(0x80 | (text & 0x3F));
                text_utf8 = tmp;
            } else if (text < 0x10000) {
                tmp[0] = static_cast<char>(0xE0 | (text >> 12));
                tmp[1] = static_cast<char>(0x80 | ((text >> 6) & 0x3F));
                tmp[2] = static_cast<char>(0x80 | (text & 0x3F));
                text_utf8 = tmp;
            } else {
                tmp[0] = static_cast<char>(0xF0 | (text >> 18));
                tmp[1] = static_cast<char>(0x80 | ((text >> 12) & 0x3F));
                tmp[2] = static_cast<char>(0x80 | ((text >> 6) & 0x3F));
                tmp[3] = static_cast<char>(0x80 | (text & 0x3F));
                text_utf8 = tmp;
            }
            if (name.empty() && text_utf8.size() == 1) {
                name = text_utf8;
            }
        }
        if (name.empty()) {
            return false; /* Modifiers, media keys: leave to the control. */
        }
        unsigned char buf[16]{};
        int n = bridge_key_encode(name.c_str(),
                                  text_utf8.empty() ? nullptr : text_utf8.c_str(),
                                  ctrl ? 1 : 0, alt ? 1 : 0, buf,
                                  sizeof buf);
        if (n <= 0) {
            return n < 0 ? false : true;
        }
        ForwardBytes(reinterpret_cast<char const *>(buf),
                     static_cast<std::size_t>(n));
        return true;
    }

    /* Converse path: every key the shared core table accepts becomes
     * child input. App shortcuts (Ctrl+N / Ctrl+Shift+N new-session
     * screen, Ctrl+R restart, Ctrl+W close) ride here too,
     * mirroring the GTK shell's app-level shortcuts. Persistence is
     * automatic (no Ctrl+S). Paste arrives via the clipboard (async);
     * the reserve rule keeps Ctrl+Shift+C/V with the native control
     * for copy.
     *
     * Two documented converse keys never reach this bubbling handler:
     * the read-only output box swallows Return (newline insertion) and
     * plain Ctrl+C (copy) before they bubble. Those ride
     * `TermBox_PreviewKeyDown` (tunneling) instead, through the same
     * core table below — one key table, no fork. */
    void MainWindow::RootGrid_KeyDown(
        IInspectable const &, KeyRoutedEventArgs const &args) {
        /* WinUI 3 KeyRoutedEventArgs carries no modifiers: query the
         * async key state directly (user32 is free to call here). */
        bool ctrl = (GetKeyState(VK_CONTROL) & 0x8000) != 0;
        bool shift = (GetKeyState(VK_SHIFT) & 0x8000) != 0;
        bool alt = (GetKeyState(VK_MENU) & 0x8000) != 0;
        int vk = static_cast<int>(args.Key());

        if (ctrl && !alt) {
            if (!shift && vk == 'N') {
                /* Ctrl+N opens the in-place new-session screen. */
                OpenNewSession();
                args.Handled(true);
                return;
            }
            if (shift && vk == 'N') {
                /* Ctrl+Shift+N opens the in-place new-session screen. */
                OpenNewSession();
                args.Handled(true);
                return;
            }
            if (!shift && vk == 'R') {
                RestartSelected();
                args.Handled(true);
                return;
            }
            if (!shift && vk == 'W') {
                CloseSelected();
                args.Handled(true);
                return;
            }
            if (!shift && vk == 'V') {
                /* Paste: clipboard text becomes child input. */
                auto data =
                    Windows::ApplicationModel::DataTransfer::Clipboard::
                        GetContent();
                if (data.Contains(
                        Windows::ApplicationModel::DataTransfer::
                            StandardDataFormats::Text())) {
                    GetContentText(data);
                }
                args.Handled(true);
                return;
            }
        }

        if (!SelectedIsLive()) {
            return;
        }
        if (EncodeForward(vk, key_char(vk, ctrl), ctrl, shift, alt)) {
            args.Handled(true);
        }
    }

    /* Tunneling converse keys for the terminal surface: the read-only
     * output box swallows Return (newline insertion) and plain Ctrl+C
     * (copy) before they can bubble to `RootGrid_KeyDown`, so without
     * this handler a prompt can be typed but never submitted and the
     * child can never be interrupted. Only these two documented keys
     * are intercepted here — everything else flows untouched, so text
     * selection, roster navigation, and the search box keep working.
     * Encoding goes through the shared core table (the same table the
     * bubble handler uses); the reserve rule stays intact because
     * Ctrl+Shift+C/V return Keep and fall through to the box. */
    void MainWindow::TermBox_PreviewKeyDown(
        IInspectable const &, KeyRoutedEventArgs const &args) {
        int vk = static_cast<int>(args.Key());
        bool isReturn = (vk == 0x0D);
        bool ctrl = (GetKeyState(VK_CONTROL) & 0x8000) != 0;
        bool shift = (GetKeyState(VK_SHIFT) & 0x8000) != 0;
        bool alt = (GetKeyState(VK_MENU) & 0x8000) != 0;
        bool isPlainCtrlC = ctrl && !shift && !alt && (vk == 'C');
        if (!isReturn && !isPlainCtrlC) {
            return;
        }
        if (!SelectedIsLive()) {
            return;
        }
        if (EncodeForward(vk, key_char(vk, ctrl), ctrl, shift, alt)) {
            args.Handled(true);
        }
    }

    /* Clipboard read is async; fire-and-forget keeps KeyDown sync. */
    fire_and_forget MainWindow::GetContentText(
        Windows::ApplicationModel::DataTransfer::DataPackageView data) {
        auto lifetime = get_strong();
        try {
            hstring text = co_await data.GetTextAsync();
            std::string utf8 = to_utf8(text);
            /* Pasted newlines go out as CR, like Return. */
            for (char &c : utf8) {
                if (c == '\n') {
                    c = '\r';
                }
            }
            ForwardBytes(utf8.c_str(), utf8.size());
        } catch (...) {
            SetStatus(L"Could not read the clipboard.");
        }
    }
}
