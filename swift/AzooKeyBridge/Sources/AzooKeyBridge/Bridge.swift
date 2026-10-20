import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

private nonisolated(unsafe) var converter: KanaKanjiConverter?
private struct SessionState {
    var candidates: [Candidate]
    var revision: UInt64
}
private nonisolated(unsafe) var sessions: [UInt64: SessionState] = [:]

private func memoryDir() -> URL {
    let dir: URL
    if let override = ProcessInfo.processInfo.environment["NOVAKEY_MEMORY_DIR"], !override.isEmpty {
        dir = URL(fileURLWithPath: override, isDirectory: true)
    } else {
        dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NovakeyR", isDirectory: true)
    }
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
    let learningType: LearningType =
        ProcessInfo.processInfo.environment["NOVAKEY_DISABLE_LEARNING"] == "1"
            ? .nothing
            : .inputAndOutput
    return .init(
        N_best: 9,
        requireJapanesePrediction: false,
        requireEnglishPrediction: false,
        keyboardLanguage: .ja_JP,
        englishCandidateInRoman2KanaInput: false,
        fullWidthRomanCandidate: false,
        halfWidthKanaCandidate: false,
        learningType: learningType,
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

// MARK: - 差分テスト用ダンプ (Rust 版 KKC への移行 P5 で撤去する)

private func hex32(_ f: Float) -> String {
    String(format: "%08x", f.bitPattern)
}

private func countDescription(_ c: ComposingCount) -> String {
    switch c {
    case .inputCount(let n): return "i\(n)"
    case .surfaceCount(let n): return "s\(n)"
    case .composite(let l, let r): return "(\(countDescription(l))+\(countDescription(r)))"
    }
}

private func dumpCandidate(_ c: Candidate) -> [String: Any] {
    [
        "text": c.text,
        "value": hex32(c.value),
        "rubyCount": c.rubyCount,
        "composingCount": countDescription(c.composingCount),
        "lastMid": c.lastMid,
        "data": c.data.map { d -> [String: Any] in
            [
                "word": d.word,
                "ruby": d.ruby,
                "lcid": d.lcid,
                "rcid": d.rcid,
                "mid": d.mid,
                "value": hex32(d.value()),
                "adjust": hex32(d.adjust),
            ]
        },
    ]
}

/// かな列を、小書きかな (ゃゅょぁぃぅぇぉっ など) を直前のかなに結合した単位に分ける。
private func kanaUnits(_ s: String) -> [String] {
    let small = Set("ゃゅょぁぃぅぇぉゎゕゖ")
    var units: [String] = []
    for ch in s {
        if small.contains(ch), !units.isEmpty {
            units[units.count - 1].append(ch)
        } else {
            units.append(String(ch))
        }
    }
    return units
}

private func convertForDump(_ reading: String) -> ConversionResult {
    var composing = ComposingText()
    composing.insertAtCursorPosition(reading, inputStyle: .direct)
    return getConverter().requestCandidates(composing, options: makeOptions())
}

@_cdecl("novakey_kkc_dump")
public func novakey_kkc_dump(
    _ corpusPtr: UnsafePointer<CChar>?, _ outPtr: UnsafePointer<CChar>?, _ typed: Bool
) -> Int32 {
    guard let corpusPtr, let outPtr else { return 1 }
    let corpusPath = String(cString: corpusPtr)
    let outPath = String(cString: outPtr)
    guard let text = try? String(contentsOfFile: corpusPath, encoding: .utf8) else { return 2 }
    var out = ""
    for rawLine in text.split(separator: "\n", omittingEmptySubsequences: true) {
        let line = rawLine.trimmingCharacters(in: .whitespaces)
        if line.isEmpty || line.hasPrefix("#") { continue }
        let converter = getConverter()
        converter.stopComposition()
        var prefixes: [String]
        var mode = typed ? "typed" : "fresh"
        var seq: [String]?
        if line.hasPrefix("typed-seq:") {
            let body = line.dropFirst("typed-seq:".count).trimmingCharacters(in: .whitespaces)
            prefixes = body.split(separator: "|", omittingEmptySubsequences: false).map(String.init)
            seq = prefixes
            if typed { mode = "typed" }  // fresh では最終の読みだけを全再計算する
        } else if typed {
            var acc = ""
            prefixes = kanaUnits(line).map { acc += $0; return acc }
        } else {
            prefixes = [line]
        }
        guard let reading = prefixes.last, !reading.isEmpty else { continue }
        var result: ConversionResult?
        if mode == "typed" {
            for p in prefixes where !p.isEmpty { result = convertForDump(p) }
        } else {
            result = convertForDump(reading)
        }
        guard let result else { continue }
        var row: [String: Any] = [
            "reading": reading,
            "mode": mode,
            "main": result.mainResults.map(dumpCandidate),
            "first_clause": result.firstClauseResults.map(dumpCandidate),
        ]
        if let seq { row["seq"] = seq }
        guard let data = try? JSONSerialization.data(
            withJSONObject: row, options: [.sortedKeys, .withoutEscapingSlashes]),
            let s = String(data: data, encoding: .utf8)
        else { return 3 }
        out += s + "\n"
    }
    do {
        try out.write(toFile: outPath, atomically: true, encoding: .utf8)
    } catch {
        return 4
    }
    return 0
}

/// Int(string) と spellOut (ja-JP) の結果を TSV でダンプする。DicdataStore.getWiseDicdata の呼び出しと同じ設定。
@_cdecl("novakey_kkc_dump_numbers")
public func novakey_kkc_dump_numbers(
    _ outPtr: UnsafePointer<CChar>?, _ widthOutPtr: UnsafePointer<CChar>?
) -> Int32 {
    guard let outPtr, let widthOutPtr else { return 1 }
    let formatter = NumberFormatter()
    formatter.numberStyle = .spellOut
    formatter.locale = .init(identifier: "ja-JP")
    var numbers: [Int] = Array(0...2000)
    var p: Int = 1
    for _ in 0...12 {
        for d in [-1, 0, 1] { numbers.append(p + d); numbers.append(-(p + d)) }
        for m in [2, 3, 5, 9] { numbers.append(p * m); numbers.append(-p * m); numbers.append(p * m - 1); numbers.append(p * m + 1) }
        p *= 10
    }
    // 1e12 の境界を越える値も含める (azooKey は ±1e12 を超えたら漢数字を出さない)
    numbers.append(contentsOf: [1_000_000_000_001, 1_000_000_000_002, 9_999_999_999_999, -1_000_000_000_001])
    // 疑似乱数 (固定シード) でサンプル
    var state: UInt64 = 0x9E3779B97F4A7C15
    func next() -> UInt64 {
        state = state &* 6364136223846793005 &+ 1442695040888963407
        return state >> 11
    }
    for _ in 0..<12000 {
        let digits = Int(next() % 12) + 1
        var v = Int(next() % UInt64(Int(pow(10.0, Double(digits)))))
        if next() % 5 == 0 { v = -v }
        numbers.append(v)
    }
    var seen = Set<Int>()
    var tsv = "input\tparsed\tkansuji\n"
    func emit(_ input: String) {
        guard let n = Int(input) else {
            tsv += "\(input)\t-\t-\n"
            return
        }
        let k: String
        if Double(n) <= 1E12 && -1E12 <= Double(n), let s = formatter.string(from: NSNumber(value: n)) {
            k = s
        } else {
            k = "-"
        }
        tsv += "\(input)\t\(n)\t\(k)\n"
    }
    for n in numbers where seen.insert(n).inserted { emit(String(n)) }
    for s in ["+5", "-5", "+0", "-0", "００", "０１２", "012", "007", "0005", "+", "-", "1_000", "1,000", " 5", "5 ", "１２", "+１", "--5", "9223372036854775807", "9223372036854775808"] {
        emit(s)
    }
    guard (try? tsv.write(toFile: String(cString: outPtr), atomically: true, encoding: .utf8)) != nil else { return 4 }

    // 全角→半角記号表 (DicdataStore.fullwidthToHalfwidth と同じ入力・同じ変換)
    let full = "＋ー＊＝・！＃％＆＇＂〜｜￡＄￥＠｀；：＜＞，．＼／＿￣－"
    let half = full.applyingTransform(.fullwidthToHalfwidth, reverse: false)!
    var wtsv = "fullwidth\thalfwidth\tfull_scalars\thalf_scalars\n"
    for (f, h) in zip(full, half) {
        let fs = f.unicodeScalars.map { String(format: "U+%04X", $0.value) }.joined(separator: " ")
        let hs = h.unicodeScalars.map { String(format: "U+%04X", $0.value) }.joined(separator: " ")
        wtsv += "\(f)\t\(h)\t\(fs)\t\(hs)\n"
    }
    wtsv += "# lengths: full=\(full.count) half=\(half.count)\n"
    guard (try? wtsv.write(toFile: String(cString: widthOutPtr), atomically: true, encoding: .utf8)) != nil else { return 5 }
    return 0
}
