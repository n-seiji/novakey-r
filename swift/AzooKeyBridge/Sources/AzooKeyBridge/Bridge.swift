import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

private nonisolated(unsafe) var converter: KanaKanjiConverter?
private struct SessionState {
    var candidates: [Candidate]
    var revision: UInt64
}
private nonisolated(unsafe) var sessions: [UInt64: SessionState] = [:]

private func memoryDir() -> URL {
    let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("NovakeyR", isDirectory: true)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

private func getConverter() -> KanaKanjiConverter {
    if let c = converter { return c }
    let c = KanaKanjiConverter.withDefaultDictionary()
    converter = c
    return c
}

private func makeOptions() -> ConvertRequestOptions {
    let dir = memoryDir()
    return .init(
        N_best: 9,
        requireJapanesePrediction: true,
        requireEnglishPrediction: false,
        keyboardLanguage: .ja_JP,
        englishCandidateInRoman2KanaInput: true,
        fullWidthRomanCandidate: false,
        halfWidthKanaCandidate: false,
        learningType: .inputAndOutput,
        maxMemoryCount: 65536,
        shouldResetMemory: false,
        memoryDirectoryURL: dir,
        sharedContainerURL: dir,
        textReplacer: .withDefaultEmojiDictionary(),
        specialCandidateProviders: KanaKanjiConverter.defaultSpecialCandidateProviders,
        metadata: .init(versionString: "NovakeyR 0.1")
    )
}

@_cdecl("novakey_kkc_convert")
public func novakey_kkc_convert(
    _ controllerId: UInt64, _ revision: UInt64,
    _ readingPtr: UnsafePointer<UInt8>?, _ readingLen: Int
) -> UnsafeMutablePointer<CChar>? {
    dispatchPrecondition(condition: .onQueue(.main))
    guard let readingPtr, readingLen > 0 else { return nil }
    let reading = String(decoding: UnsafeBufferPointer(start: readingPtr, count: readingLen), as: UTF8.self)
    var composing = ComposingText()
    composing.insertAtCursorPosition(reading, inputStyle: .direct)
    let result = getConverter().requestCandidates(composing, options: makeOptions())
    // mainResults は N_best を超える予測・部分候補も含むため上位のみ使う。
    // 学習(select)の engine_index と整合するよう、保存側も同じ配列で切り詰める
    let top = Array(result.mainResults.prefix(9))
    sessions[controllerId] = SessionState(candidates: top, revision: revision)
    let joined = top
        .map { $0.text.replacingOccurrences(of: "\u{1F}", with: "") }
        .joined(separator: "\u{1F}")
    return strdup(joined)
}

@_cdecl("novakey_kkc_select")
public func novakey_kkc_select(
    _ controllerId: UInt64, _ revision: UInt64, _ engineIndex: Int
) -> Bool {
    dispatchPrecondition(condition: .onQueue(.main))
    defer {
        converter?.stopComposition()
        sessions[controllerId] = nil
    }
    guard let session = sessions[controllerId],
          session.revision == revision,
          engineIndex >= 0, engineIndex < session.candidates.count
    else { return false }
    let c = getConverter()
    c.updateLearningData(session.candidates[engineIndex])
    c.commitUpdateLearningData()
    return true
}

@_cdecl("novakey_kkc_cancel")
public func novakey_kkc_cancel(_ controllerId: UInt64) {
    dispatchPrecondition(condition: .onQueue(.main))
    converter?.stopComposition()
    sessions[controllerId] = nil
}
