---
name: ios-swift-standards
description: Use when writing, editing, or reviewing Swift or SwiftUI code in apps/student-ios. Covers Swift API design guidelines, async/await, SwiftUI view patterns, WKWebView bridges, AVFoundation/Speech/Camera, Swift Testing, SwiftLint rules, and Info.plist privacy keys. Load before editing any .swift file.
---

# iOS + Swift + SwiftUI standards for HomeTutor

The full sourced rules are in `docs/ios-best-practices-research.md`. **Read that file before writing or editing iOS code.** Below is the index.

## What the research doc covers (by section)

1. **Swift API Design Guidelines** — naming (camelCase methods/props, UpperCamelCase types, omit needless words), label the role not the type, "call site reads like a sentence", `mutating`, `Protocol` naming, `as?`/`as!` rules, `try?`/`try!` rules, escaping vs non-escaping closures.
2. **The Swift Programming Language** — error handling (`throws`/`try`/`catch`, `Error` protocol, `Result`, `rethrows`), optionals (avoid `!` unless programmer error, `guard let` for early exit, `map`/`flatMap` on Optionals), value vs reference types (`struct` for value, `class` for identity), closures (capture semantics, `weak`/`unowned` for cycles, `@escaping`), `Sendable`/`@MainActor`/`async`/`await`/`Task`/`TaskGroup`, `actor` for shared mutable state, structured vs unstructured concurrency.
3. **SwiftUI** — views are structs + value types, `var body: some View`, view modifier order matters, `@State`/`@StateObject`/`@ObservedObject`/`@EnvironmentObject`/`@Binding`, no side effects in `body`, stable `id` in `ForEach` (NOT array index), `Equatable` to skip re-renders, `NavigationStack`/`.sheet`/`.fullScreenCover`/`.navigationDestination` over deprecated `NavigationLink(destination:)`, `Observable` macro (iOS 17+) vs `ObservableObject`.
4. **Apple HIG** — sheets vs full-screen, `NavigationStack` vs `TabView`, safe-area & dynamic type, system components over custom, accessibility (`accessibilityLabel`/`accessibilityHint`/`accessibilityValue`, dynamic type, reduce motion, voice-over), keyboard handling (`safeAreaPadding`/`.scrollDismissesKeyboard`).
5. **Swift Concurrency** — `async let` for parallel work, `Task {}` for fire-and-forget (capture context), `TaskGroup` for dynamic parallelism, `@MainActor` default for SwiftUI views, `nonisolated` to opt out, `actor` for shared mutable state, `Sendable` for cross-actor values, prefer `@MainActor`/`await MainActor.run` over `DispatchQueue.main.async`, cancellation via `Task.cancel()`/`checkCancellation()`, `withCheckedThrowingContinuation` for bridging callbacks (never call continuation more than once).
6. **WKWebView / Web bridges** — `WKUserContentController` + `add(_:name:)` for JS→Swift, `weak`-retain the handler (memory leak trap), `evaluateJavaScript` for Swift→JS, `WKURLSchemeHandler` for custom schemes, `decidePolicyFor` must call decisionHandler, ContentWorld isolation, `httpOnly` cookie caveat, don't block main thread for JS evaluation.
7. **AVFoundation / Speech / Camera** — `SFSpeechRecognizer` requires `requestAuthorization`, `SFSpeechRecognitionTask` cancellation/timeout, audio session categories, `AVCaptureSession` config off main thread (`beginConfiguration`/`commitConfiguration`), never block the capture callback queue, `Info.plist` privacy keys (`NSMicrophoneUsageDescription`, `NSCameraUsageDescription`, `NSSpeechRecognitionUsageDescription`, `NSPhotoLibraryUsageDescription`) — missing them = hard crash.
8. **iOS security / persistence** — `NSAppTransportSecurity` (no `NSAllowsArbitraryLoads` in prod), Keychain for secrets (not `UserDefaults`), `UserDefaults` for user prefs, `Codable` for persisted structs.
9. **Testing** — Swift Testing framework (`@Test`, `#expect`, `#require`, traits, parallel by default, parameterized) over XCTest for new tests, UI tests vs unit tests, `XCTestExpectation` for async work, swift-snapshot-testing for snapshots.
10. **SwiftPM** — `Package.swift` manifest, targets + products, prefer SPM over CocoaPods, `@testTarget` deps, resources, `Package.resolved` for reproducibility.
11. **SwiftLint rules** — ~15-25 rules worth enabling: `force_unwrapping`, `force_try`, `force_cast`, `implicitly_unwrapped_optional`, `cyclomatic_complexity`, `switch_case_on_newline`, `trailing_semicolon`, `redundant_optional_assignment`, `empty_count`, `first_where`, `contains_over_filter_is_empty`, `unowned_variable_capture`, `weak_delegate`.

## How to use this

When editing iOS code:
- Read the relevant section(s) from `docs/ios-best-practices-research.md` for the construct you're working with.
- Follow the existing codebase conventions first; the standards doc second.
- After editing, build via `xcodebuild -scheme HomeTutor -destination 'platform=iOS Simulator,name=iPhone 15'` if Xcode is available; otherwise flag that you can't verify.
