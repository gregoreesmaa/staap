// Main window: roster + ConPTY-backed terminal over the core C ABI.
// See MainWindow.xaml.cpp for the epic-DoD wiring table.
#pragma once

#include "MainWindow.g.h"
#include "MainWindow.xaml.g.h"

#include "core_bridge.h"

namespace winrt::StaapWinUI::implementation
{
    /* Display cache for one roster row: the last full screen text (feed
     * delta base) plus everything fed to the view (capped). The core
     * registry owns the PTYs — this only caches view state per row id. */
    struct DisplayRow
    {
        std::string last_snapshot;
        std::string shown;
    };

    struct MainWindow : MainWindowT<MainWindow>
    {
        MainWindow();
        ~MainWindow();

        void NewButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::RoutedEventArgs const &args);
        /* SplitButton primary-face entry: same instant repeat-last as
         * NewButton_Click (which stays for the menu item + Ctrl+N). */
        void NewSplitButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::SplitButtonClickEventArgs const
                &args);
        void RestartButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::RoutedEventArgs const &args);
        void CloseButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::RoutedEventArgs const &args);
        /* Instant repeat-last shared by the face, the menu item, and
         * Ctrl+N: one path, no divergence. */
        void RepeatLastSession();
        /* Restart/resume + close for the selected run (Ctrl+R / Ctrl+W,
         * parity with the `r`/`x` keys and the other shells). */
        void RestartSelected();
        void CloseSelected();
        /* 2D new-session picker (folder x CLI + tri-state yolo): the
         * caret/menu counterpart to NewButton_Click's instant repeat. */
        void PickButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::RoutedEventArgs const &args);
        fire_and_forget PickNewSessionAsync();
        void FilterBox_TextChanged(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::TextChangedEventArgs const &args);
        void Roster_SelectionChanged(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::SelectionChangedEventArgs const
                &args);
        /* History expander toggle (XAML-wired handlers must be public):
         * core-owned expansion + persist; programmatic sync in
         * RefreshRoster runs under m_syncing so it never writes back.
         * The Expander control fires distinct Expanding/Collapsed events
         * with their own arg types, so one shared handler cannot bind
         * both — both funnels below share WriteHistoryExpanded. */
        void HistoryExpander_Expanding(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::ExpanderExpandingEventArgs const
                &args);
        void HistoryExpander_Collapsed(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::ExpanderCollapsedEventArgs const
                &args);
        void SidebarThumb_DragDelta(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Controls::Primitives::DragDeltaEventArgs const
                &args);
        void SidebarThumb_KeyDown(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Input::KeyRoutedEventArgs const &args);
        void RootGrid_KeyDown(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Input::KeyRoutedEventArgs const &args);
        void TermBox_PreviewKeyDown(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::Input::KeyRoutedEventArgs const &args);
        /* Empty-overlay button (XAML-wired handlers must be public). */
        void EmptyButton_Click(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::RoutedEventArgs const &args);

    private:
        void OnTick(
            Windows::Foundation::IInspectable const &sender,
            Windows::Foundation::IInspectable const &args);
        void OnClosed(
            Windows::Foundation::IInspectable const &sender,
            Microsoft::UI::Xaml::WindowEventArgs const &args);
        void RefreshRoster();
        void ShowSelected();
        void ShowEmpty(std::wstring const &title, std::wstring const &detail,
                       std::wstring const &button);
        long long RowIndexById(std::wstring const &id);
        /* Group-list helpers (issue #73): the four lists share one
         * selection, kept in the core (`staap_selected`); m_syncing guards
         * the SelectionChanged fan-out while the selection is moved. */
        void RebuildGroupList(
            Microsoft::UI::Xaml::Controls::ListView const &list,
            std::vector<std::pair<std::wstring, std::wstring>> const &rows);
        void SelectRowById(std::wstring const &id);
        void WriteHistoryExpanded(bool expanded);
        bool FirstRowId(std::wstring &id);
        void SetStatus(winrt::hstring const &text);
        void ForwardBytes(char const *data, std::size_t len);
        /* Shared core-table encode + forward (true = handled). */
        bool EncodeForward(int vk, char32_t text, bool ctrl, bool shift,
                           bool alt);
        /* Selected roster row id (wide copy of the core selection). */
        std::wstring SelectedId();
        /* True when the selected row owns a live PTY in the core. */
        bool SelectedIsLive();
        /* Resizable sidebar: read/apply helpers for the SidebarColumn
         * width behind the Thumb grip. */
        double SidebarWidthPx();
        void SetSidebarWidth(double w);
        fire_and_forget GetContentText(
            Windows::ApplicationModel::DataTransfer::DataPackageView data);

        ::StaapCore *m_core{nullptr};
        /* Roster row id -> display cache (feed base + shown text). The
         * core registry owns the PTYs; the selection/filter live in the
         * core too (`staap_selected`/`staap_set_filter`) — this shell only
         * renders what the core reports. */
        std::map<std::wstring, DisplayRow> m_rows;
        bool m_syncing{false}; /* true while moving shared selection */
        /* Empty-overlay button mode: 0 = New Session, 1 = restart/resume,
         * 2 = no button. Set by ShowSelected, read by EmptyButton_Click. */
        int m_emptyMode{2};
        std::string m_filter;                   /* sidebar filter (UTF-8) */
        std::string m_fingerprint; /* roster rebuild gate */
        Microsoft::UI::Dispatching::DispatcherQueueTimer m_timer{nullptr};
        winrt::event_token m_closedToken{};
    };
}

namespace winrt::StaapWinUI::factory_implementation
{
    struct MainWindow : MainWindowT<MainWindow, implementation::MainWindow>
    {
    };
}
