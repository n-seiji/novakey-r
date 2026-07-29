// swift-tools-version: 6.1
import PackageDescription

let package = Package(
    name: "AzooKeyBridge",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "AzooKeyBridge", type: .static, targets: ["AzooKeyBridge"])
    ],
    dependencies: [
        .package(url: "https://github.com/azooKey/AzooKeyKanaKanjiConverter", exact: "0.11.2")
    ],
    targets: [
        .target(name: "AzooKeyBridge", dependencies: [
            .product(name: "KanaKanjiConverterModuleWithDefaultDictionary", package: "AzooKeyKanaKanjiConverter")
        ])
    ]
)
