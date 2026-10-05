#pragma once

// Precompiled header for the WinUI 3 shell (issue #64). Standard
// C++/WinRT + WinAppSDK set; project-local and core bridge headers are
// included at the top of each .cpp instead so incremental edits to them
// do not rebuild the world.

#include <windows.h>
#include <unknwn.h>
#include <restrictederrorinfo.h>
#include <hstring.h>

// WinRT projections used by the shell.
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.ApplicationModel.Activation.h>
#include <winrt/Windows.ApplicationModel.DataTransfer.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Windows.Storage.Pickers.h>
#include <winrt/Windows.System.h>
#include <winrt/Microsoft.UI.h>
#include <winrt/Microsoft.UI.Dispatching.h>
#include <winrt/Microsoft.UI.Windowing.h>
#include <winrt/Microsoft.UI.Xaml.h>
#include <winrt/Microsoft.UI.Xaml.Controls.h>
#include <winrt/Microsoft.UI.Xaml.Controls.Primitives.h>
#include <winrt/Microsoft.UI.Xaml.Data.h>
#include <winrt/Microsoft.UI.Xaml.Input.h>
#include <winrt/Microsoft.UI.Xaml.Media.h>
#include <winrt/Microsoft.UI.Xaml.Navigation.h>

// XAML implementation types for the generated type-info provider
// (XamlTypeInfo.g.cpp references implementation::App/MainWindow but
// includes no project headers of its own).
#include "App.xaml.h"
#include "MainWindow.xaml.h"

// Standard C++ used across the code-behind.
#include <cstdlib>
#include <map>
#include <string>
#include <vector>
