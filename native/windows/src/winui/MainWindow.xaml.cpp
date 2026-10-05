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
//   spawn       New Session button / Ctrl+N -> bridge_run_spawn
//               (repeat-last: null CLI/folder, zero yolo); picker button /
//               Ctrl+Shift+N -> ContentDialog (folder x CLI + yolo) below
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
            RepeatLastSession();
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

    /* New Session repeats the last launch instantly through the core
     * bridge (the null-CLI/null-folder/zero-yolo form resolves the
     * effective default, so a picker-confirmed combo repeats here).
     * The fresh PTY mints a shell-local terminal like the empty-roster
     * path already did — one click always opens something and the empty
     * roster is a starting point, not a dead end. A live local stays
     * put (no orphan duplicates): it has no roster row to return to. */
    void MainWindow::NewButton_Click(IInspectable const &,
                                     RoutedEventArgs const &) {
        RepeatLastSession();
    }

    void MainWindow::NewSplitButton_Click(
        IInspectable const &,
        Controls::SplitButtonClickEventArgs const &) {
        RepeatLastSession();
    }

    void MainWindow::RestartButton_Click(IInspectable const &,
                                         RoutedEventArgs const &) {
        RestartSelected();
    }

    void MainWindow::CloseButton_Click(IInspectable const &,
                                       RoutedEventArgs const &) {
        CloseSelected();
    }

    /* Instant repeat-last shared by the SplitButton face, its menu item,
     * and Ctrl+N: one path, no divergence. The core registry attaches
     * the spawn to a new roster row under the shared cap — no
     * shell-local terminals, so every run rows in the roster and
     * statuses actually move. */
    void MainWindow::RepeatLastSession() {
        if (!m_core) {
            return;
        }
        char id[256] = {0};
        char *err = nullptr;
        if (bridge_run_spawn(m_core, nullptr, nullptr, 0, kCols, kRows,
                             id, sizeof id, &err) != 0) {
            std::string msg = "Could not spawn: ";
            msg += err ? err : "unknown error";
            SetStatus(to_hstring(msg));
            free(err);
            return;
        }
        /* Force the roster rebuild (new rows attach in the core now). */
        m_fingerprint.clear();
        RefreshRoster();
        ShowSelected();
        /* Hand the keyboard to the new session (takes the keyboard on
         * spawn, like the macOS shell's `n` key): without this, focus stays on
         * the New Session button, where Return re-clicks instead of
         * submitting the typed prompt. */
        TermBox().Focus(Microsoft::UI::Xaml::FocusState::Programmatic);
        char *eff_raw = bridge_effective_cli(m_core, nullptr);
        std::string eff = eff_raw ? eff_raw : "terminal";
        bridge_string_free(eff_raw);
        SetStatus(hstring{to_wide("New " + eff + " session started.")});
    }

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

    /* Picker button: open the 2D new-session ContentDialog (folder x
     * CLI + tri-state yolo) over the fresh catalog + recents. */
    void MainWindow::PickButton_Click(IInspectable const &,
                                      RoutedEventArgs const &) {
        PickNewSessionAsync();
    }
    /* 2D new-session dialog. Folder: Browse… (folder picker) or a
     * TextBox (blank = inherit) plus the persisted recents; a missing
     * folder is reported in the status line with the fix named. CLI:
     * a ComboBox over the catalog; missing CLIs render disabled with
     * an install hint, never hidden. Yolo: a tri-state ComboBox
     * (Default / On once / Off once), safe by default; the preview line
     * names the exact combination before Spawn. */
    fire_and_forget MainWindow::PickNewSessionAsync() {
        auto lifetime = get_strong();
        if (!m_core) {
            co_return;
        }
        char *clis_raw = bridge_clis_json();
        std::string clis_json = clis_raw ? clis_raw : "[]";
        bridge_string_free(clis_raw);
        char *recents_raw = bridge_recent_json(m_core);
        std::string recents_json = recents_raw ? recents_raw : "[]";
        bridge_string_free(recents_raw);
        auto clis = picker::parse_clis(clis_json);
        auto recents = picker::parse_recents(recents_json);
        if (clis.empty()) {
            SetStatus(L"No agent CLI catalog: cannot open the picker.");
            co_return;
        }

        ComboBox cliBox;
        for (auto const &cli : clis) {
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
            cliBox.Items().Append(item);
        }
        /* Preselect the first available CLI (catalog order). */
        for (uint32_t i = 0; i < cliBox.Items().Size(); ++i) {
            auto item = cliBox.Items().GetAt(i).try_as<ComboBoxItem>();
            if (item && item.IsEnabled()) {
                cliBox.SelectedIndex(static_cast<int32_t>(i));
                break;
            }
        }

        // Owner window for the folder picker (issue #102).
        HWND hwnd{ nullptr };
        if (auto native = this->try_as<IWindowNative>()) {
            native->get_WindowHandle(&hwnd);
        }

        TextBox folderBox;
        folderBox.PlaceholderText(L"Blank = current folder");
        folderBox.Text(to_wide(recents.empty() ? "" : recents[0]));

        ComboBox recentBox;
        if (!recents.empty()) {
            recentBox.Items().Append(box_value(winrt::hstring(L"Type a folder…")));
            for (auto const &r : recents) {
                recentBox.Items().Append(box_value(to_wide(r)));
            }
            recentBox.SelectedIndex(0);
        }

        ComboBox yoloBox;
        yoloBox.Items().Append(box_value(winrt::hstring(L"Default")));
        yoloBox.Items().Append(box_value(winrt::hstring(L"On (once)")));
        yoloBox.Items().Append(box_value(winrt::hstring(L"Off (once)")));
        yoloBox.SelectedIndex(0);
        ToolTipService::SetToolTip(
            yoloBox, box_value(winrt::hstring(
                          L"Yolo lets the agent run commands without asking. "
                          L"Default follows the per-agent config; once-choices "
                          L"apply to this run only and are never saved.")));

        TextBlock preview;
        preview.Style(Application::Current()
                          .Resources()
                          .Lookup(box_value(L"CaptionTextBlockStyle"))
                          .as<Style>());
        auto refresh = [&]() {
            int ci = cliBox.SelectedIndex();
            std::string cli =
                (ci >= 0 && static_cast<size_t>(ci) < clis.size())
                    ? clis[static_cast<size_t>(ci)].id
                    : "muse";
            /* Label "Default" with its resolved state (issue #102). */
            std::string def_label = "Default (";
            def_label += bridge_yolo_default(m_core, cli.c_str()) ? "on" : "off";
            def_label += ")";
            yoloBox.Items().SetAt(0, box_value(to_wide(def_label)));
            std::string folder = to_utf8(folderBox.Text());
            /* Core-owned preview + yolo mapping (single copies). */
            int yolo = bridge_yolo_value(yoloBox.SelectedIndex());
            char *text =
                bridge_spawn_preview(cli.c_str(), folder.c_str(), yolo);
            preview.Text(to_wide(text ? text : ""));
            free(text);
        };
        cliBox.SelectionChanged(
            [&refresh](IInspectable const &, SelectionChangedEventArgs const &) {
                refresh();
            });
        yoloBox.SelectionChanged(
            [&refresh](IInspectable const &, SelectionChangedEventArgs const &) {
                refresh();
            });
        folderBox.TextChanged(
            [&](IInspectable const &, TextChangedEventArgs const &) {
                refresh();
            });
        if (!recents.empty()) {
            recentBox.SelectionChanged(
                [&](IInspectable const &,
                    SelectionChangedEventArgs const &) {
                    int ri = recentBox.SelectedIndex();
                    if (ri > 0 &&
                        static_cast<size_t>(ri - 1) < recents.size()) {
                        folderBox.Text(to_wide(recents[static_cast<size_t>(ri - 1)]));
                    }
                    refresh();
                });
        }
        refresh();

        StackPanel panel;
        panel.Spacing(8);
        auto head = [](const wchar_t *t) {
            TextBlock h;
            h.Text(t);
            h.Style(Application::Current()
                        .Resources()
                        .Lookup(box_value(L"SubtitleTextBlockStyle"))
                        .as<Style>());
            return h;
        };
        panel.Children().Append(head(L"Where should it work?"));
        StackPanel folderRow;
        folderRow.Orientation(Orientation::Horizontal);
        folderRow.Spacing(8);
        folderBox.MinWidth(260);
        Button browse;
        browse.Content(box_value(L"Browse…"));
        browse.Click([folderBox, hwnd](IInspectable const &,
                                       RoutedEventArgs const &)
                       -> fire_and_forget {
            // Issue #102: pick the folder in the UI instead of typing it.
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
        });
        folderRow.Children().Append(folderBox);
        folderRow.Children().Append(browse);
        panel.Children().Append(folderRow);
        if (!recents.empty()) {
            panel.Children().Append(recentBox);
        }
        panel.Children().Append(head(L"Who should do it?"));
        panel.Children().Append(cliBox);
        panel.Children().Append(head(L"Permission mode"));
        panel.Children().Append(yoloBox);
        panel.Children().Append(preview);

        ContentDialog dialog;
        dialog.Title(box_value(winrt::hstring(L"Start a new run")));
        dialog.Content(panel);
        dialog.PrimaryButtonText(L"Spawn");
        dialog.CloseButtonText(L"Cancel");
        dialog.DefaultButton(ContentDialogButton::Primary);
        dialog.XamlRoot(this->Content().XamlRoot());
        /* Spawn stays disabled while a missing CLI is selected: the
         * failure would be certain, so prevent it inline (fail visible
         * at the control, not after the click). */
        auto sync_spawn = [&]() {
            int ci = cliBox.SelectedIndex();
            bool avail =
                (ci >= 0 && static_cast<size_t>(ci) < clis.size()) &&
                clis[static_cast<size_t>(ci)].available;
            dialog.IsPrimaryButtonEnabled(avail);
        };
        cliBox.SelectionChanged(
            [&sync_spawn](IInspectable const &, SelectionChangedEventArgs const &) {
                sync_spawn();
            });
        sync_spawn();
        auto result = co_await dialog.ShowAsync();
        if (result != ContentDialogResult::Primary) {
            co_return;
        }
        int ci = cliBox.SelectedIndex();
        std::string cli =
            (ci >= 0 && static_cast<size_t>(ci) < clis.size())
                ? clis[static_cast<size_t>(ci)].id
                : "muse";
        std::string raw_folder = to_utf8(folderBox.Text());
        /* Blank/whitespace inherits (core trims the same way). */
        std::string folder;
        {
            size_t b = raw_folder.find_first_not_of(" \t\n\r");
            if (b != std::string::npos) {
                size_t e = raw_folder.find_last_not_of(" \t\n\r");
                folder = raw_folder.substr(b, e - b + 1);
            }
        }
        /* is-dir check inline (fail visible, dialog already closed by
         * ShowAsync: report in the status line with the fix named). */
        if (!folder.empty()) {
            DWORD attrs = GetFileAttributesA(folder.c_str());
            if (attrs == INVALID_FILE_ATTRIBUTES ||
                !(attrs & FILE_ATTRIBUTE_DIRECTORY)) {
                SetStatus(winrt::hstring(to_wide("No such folder: " + folder +
                                                 " — reopen the picker to fix it.")));
                co_return;
            }
        }
        /* Core-owned yolo mapping; the registry spawn attaches a real
         * roster row under the shared cap (a refusal names per-run
         * close, like every other shell). */
        int yolo = bridge_yolo_value(yoloBox.SelectedIndex());
        char id[256] = {0};
        char *err = nullptr;
        if (bridge_run_spawn(m_core, cli.c_str(),
                             folder.empty() ? nullptr : folder.c_str(),
                             yolo, kCols, kRows, id, sizeof id, &err) != 0) {
            std::string msg = "Could not spawn: ";
            msg += err ? err : "unknown error";
            SetStatus(to_hstring(msg));
            free(err);
            co_return;
        }
        m_fingerprint.clear();
        RefreshRoster();
        ShowSelected();
        char *done = bridge_spawn_preview(cli.c_str(), folder.c_str(), yolo);
        SetStatus(winrt::hstring(to_wide(done ? done : "")));
        free(done);
        /* Hand the keyboard to the new session (takes the keyboard on
         * spawn, like the macOS shell's `n` key): without this, focus stays on
         * the New Session button, where Return re-clicks instead of
         * submitting the typed prompt. */
        TermBox().Focus(Microsoft::UI::Xaml::FocusState::Programmatic);
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
     * child input. App shortcuts (Ctrl+N repeat-last spawn,
     * Ctrl+Shift+N picker, Ctrl+R restart, Ctrl+W close) ride here too,
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
                /* Ctrl+N repeats the last launch instantly. */
                NewButton_Click(nullptr, nullptr);
                args.Handled(true);
                return;
            }
            if (shift && vk == 'N') {
                /* Ctrl+Shift+N opens the full picker. */
                PickButton_Click(nullptr, nullptr);
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
