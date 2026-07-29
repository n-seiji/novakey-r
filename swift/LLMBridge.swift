import Foundation
import FoundationModels

@_silgen_name("novakey_llm_on_result")
func novakey_llm_on_result(
    _ controllerId: UInt64, _ generation: UInt64,
    _ status: Int32, _ textPtr: UnsafePointer<UInt8>?, _ textLen: Int)

private let instructions = """
あなたは日本語入力システムの候補リランキング器である。ひらがな読みと候補リストを見て、最も自然な候補の番号だけを半角数字で返せ。候補にない文字列を生成してはいけない。
"""

private var latestGenerations: [UInt64: UInt64] = [:]
private var inFlightTasks: [UInt64: Task<Void, Never>] = [:]

@MainActor
private func registerLatest(_ id: UInt64, _ gen: UInt64) {
    latestGenerations[id] = gen
    inFlightTasks[id]?.cancel()
}

@MainActor
private func isLatest(_ id: UInt64, _ gen: UInt64) -> Bool {
    latestGenerations[id] == gen
}

@MainActor
private func trackTask(_ id: UInt64, _ gen: UInt64, _ task: Task<Void, Never>) {
    if latestGenerations[id] == gen {
        inFlightTasks[id] = task
    }
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

private func deliver(_ id: UInt64, _ gen: UInt64, _ status: Int32, _ text: String) {
    DispatchQueue.main.async {
        guard latestGenerations[id] == gen else { return }
        var bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            novakey_llm_on_result(id, gen, status, buf.baseAddress, buf.count)
        }
    }
}

private func buildPrompt(reading: String, candidates: [String]) -> String {
    let numbered = candidates.enumerated().map { "\($0.offset): \($0.element)" }.joined(separator: "\n")
    return "読み: \(reading)\n候補:\n\(numbered)\n最適な候補番号だけを返答:"
}

private func parseIndex(_ text: String, count: Int) -> Int? {
    let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
    guard let first = trimmed.split(whereSeparator: { !$0.isNumber }).first,
          let index = Int(first),
          index >= 0,
          index < count else { return nil }
    return index
}

@_cdecl("novakey_llm_rerank")
public func novakey_llm_rerank(
    _ controllerId: UInt64, _ generation: UInt64,
    _ readingPtr: UnsafePointer<UInt8>, _ readingLen: Int,
    _ candsPtr: UnsafePointer<UInt8>, _ candsLen: Int
) {
    let reading = String(decoding: UnsafeBufferPointer(start: readingPtr, count: readingLen), as: UTF8.self)
    let joined = String(decoding: UnsafeBufferPointer(start: candsPtr, count: candsLen), as: UTF8.self)
    let candidates = joined.split(separator: "\u{1F}", omittingEmptySubsequences: false).map(String.init)
    guard candidates.count >= 2 else { return }
    DispatchQueue.main.async {
        registerLatest(controllerId, generation)
        let session = LanguageModelSession(instructions: instructions)
        let prompt = buildPrompt(reading: reading, candidates: candidates)
        let task = Task {
            do {
                let response = try await session.respond(to: prompt)
                guard await isLatest(controllerId, generation), !Task.isCancelled else { return }
                let output = response.content.trimmingCharacters(in: .whitespacesAndNewlines)
                if let index = parseIndex(output, count: candidates.count) {
                    deliver(controllerId, generation, 0, String(index))
                } else {
                    deliver(controllerId, generation, 1, output)
                }
            } catch {
                guard await isLatest(controllerId, generation), !Task.isCancelled else { return }
                deliver(controllerId, generation, 1, "\(error)")
            }
            await clearInFlightTask(controllerId, generation)
        }
        trackTask(controllerId, generation, task)
    }
}
