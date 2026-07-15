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

// controller ごとにセッションを分離(文脈の混線防止)。メインスレッドからのみ触る。
private var sessions: [UInt64: LanguageModelSession] = [:]

private func session(for id: UInt64) -> LanguageModelSession {
    if let s = sessions[id] { return s }
    let s = LanguageModelSession(instructions: instructions)
    s.prewarm()
    sessions[id] = s
    return s
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

private func deliver(_ id: UInt64, _ gen: UInt64, _ status: Int32, _ text: String) {
    DispatchQueue.main.async {
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
    // セッション辞書はメインスレッド専用なので、取得を main で行ってから Task へ
    DispatchQueue.main.async {
        let s = session(for: controllerId)
        let prompt = ctx.isEmpty ? "入力: \(input) →" : "直前の文脈: \(ctx)\n入力: \(input) →"
        Task {
            do {
                let response = try await s.respond(to: prompt)
                deliver(controllerId, generation, 0, response.content.trimmingCharacters(in: .whitespacesAndNewlines))
            } catch let e as LanguageModelSession.GenerationError {
                // コンテキスト超過は専用コード。セッションを作り直して次回に備える
                DispatchQueue.main.async { sessions[controllerId] = nil }
                if case .exceededContextWindowSize = e {
                    deliver(controllerId, generation, 2, "")
                } else {
                    deliver(controllerId, generation, 1, "\(e)")
                }
            } catch {
                deliver(controllerId, generation, 1, "\(error)")
            }
        }
    }
}
