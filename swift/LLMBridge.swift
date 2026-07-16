// LLMBridge.swift - FoundationModels を C ABI で Rust に公開する。
// 契約: novakey_llm_on_result は必ず main queue 上で呼ぶ。
import Foundation
import FoundationModels

@_silgen_name("novakey_llm_on_result")
func novakey_llm_on_result(
    _ controllerId: UInt64, _ generation: UInt64,
    _ status: Int32, _ textPtr: UnsafePointer<UInt8>?, _ textLen: Int)

private let instructions = """
あなたは日本語入力システムの変換エンジンである。入力されたひらがな読み列を、\
文脈に合った自然な日本語表記(漢字・カタカナ・英字を適切に使用)に変換し、\
適切な句読点(。、)を補って出力せよ。読みに存在しない内容を追加しない。\
変換結果の文字列のみを返す。

例:
入力: きょうはいいてんきですね → 今日はいい天気ですね。
入力: あっぷるのはっぴょうかいをみた → Appleの発表会を見た。
入力: それでかえったあとにねた → それで、帰った後に寝た。
"""

// controller ごとの最新世代と実行中 Task。メインスレッドからのみ触る。
private var latestGenerations: [UInt64: UInt64] = [:]
private var inFlightTasks: [UInt64: Task<Void, Never>] = [:]

@MainActor
private func isLatest(_ id: UInt64, _ gen: UInt64) -> Bool {
    latestGenerations[id] == gen
}

@MainActor
private func clearInFlightTask(_ id: UInt64, _ gen: UInt64) {
    if latestGenerations[id] == gen {
        inFlightTasks[id] = nil
    }
}

@_cdecl("novakey_llm_availability")
public func novakey_llm_availability() -> Int32 {
    switch SystemLanguageModel.default.availability {
    case .available: return 0
    case .unavailable(.deviceNotEligible): return 1
    case .unavailable(.appleIntelligenceNotEnabled): return 2
    case .unavailable(.modelNotReady): return 3
    default: return -1
    }
}

private func containsIdeograph(_ text: String) -> Bool {
    text.unicodeScalars.contains { scalar in
        (0x3400...0x4DBF).contains(scalar.value) ||
            (0x4E00...0x9FFF).contains(scalar.value) ||
            (0xF900...0xFAFF).contains(scalar.value)
    }
}

private func normalizedReading(_ text: String) -> String? {
    let mutable = NSMutableString(string: text)
    CFStringTransform(mutable as CFMutableString, nil, "Any-Hiragana" as CFString, false)
    let transformed = mutable as String
    if containsIdeograph(transformed) { return nil }

    var scalars = String.UnicodeScalarView()
    for scalar in transformed.unicodeScalars {
        switch scalar.value {
        case 0x3041...0x3096, 0x30A1...0x30FA, 0x0030...0x0039, 0x0041...0x005A, 0x0061...0x007A:
            scalars.append(scalar)
        default:
            continue
        }
    }
    return String(scalars)
}

private func isPlausibleConversion(input: String, output: String) -> Bool {
    guard let inputReading = normalizedReading(input),
          let outputReading = normalizedReading(output),
          !inputReading.isEmpty, !outputReading.isEmpty else {
        return true
    }

    let ratio = Double(outputReading.count) / Double(inputReading.count)
    return ratio >= 0.7 && ratio <= 1.3
}

private func deliver(_ id: UInt64, _ gen: UInt64, _ status: Int32, _ text: String) {
    DispatchQueue.main.async {
        guard latestGenerations[id] == gen else {
            return
        }

        var bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            novakey_llm_on_result(id, gen, status, buf.baseAddress, buf.count)
        }
    }
}

@_cdecl("novakey_llm_convert")
public func novakey_llm_convert(
    _ controllerId: UInt64, _ generation: UInt64,
    _ inputPtr: UnsafePointer<UInt8>, _ inputLen: Int,
    _ ctxPtr: UnsafePointer<UInt8>, _ ctxLen: Int
) {
    let input = String(decoding: UnsafeBufferPointer(start: inputPtr, count: inputLen), as: UTF8.self)
    let ctx = String(decoding: UnsafeBufferPointer(start: ctxPtr, count: ctxLen), as: UTF8.self)
    // 世代管理はメインスレッド専用。新しい要求が来たら古い要求を捨てる。
    DispatchQueue.main.async {
        latestGenerations[controllerId] = generation
        inFlightTasks[controllerId]?.cancel()

        let prompt = ctx.isEmpty ? "入力: \(input) →" : "直前の文脈: \(ctx)\n入力: \(input) →"
        let task = Task {
            guard await isLatest(controllerId, generation), !Task.isCancelled else {
                return
            }

            let s = LanguageModelSession(instructions: instructions)
            s.prewarm()

            do {
                let response = try await s.respond(to: prompt)
                guard await isLatest(controllerId, generation), !Task.isCancelled else {
                    return
                }

                let output = response.content.trimmingCharacters(in: .whitespacesAndNewlines)
                if isPlausibleConversion(input: input, output: output) {
                    deliver(controllerId, generation, 0, output)
                } else {
                    deliver(controllerId, generation, 1, "")
                }
            } catch let e as LanguageModelSession.GenerationError {
                guard await isLatest(controllerId, generation), !Task.isCancelled else {
                    return
                }

                // コンテキスト超過は専用コード。
                if case .exceededContextWindowSize = e {
                    deliver(controllerId, generation, 2, "")
                } else {
                    deliver(controllerId, generation, 1, "\(e)")
                }
            } catch {
                guard await isLatest(controllerId, generation), !Task.isCancelled else {
                    return
                }

                deliver(controllerId, generation, 1, "\(error)")
            }

            await clearInFlightTask(controllerId, generation)
        }
        inFlightTasks[controllerId] = task
    }
}
