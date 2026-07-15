// A lightweight block-level Markdown renderer for the Inspector's Describe tab. The AI
// descriptions frequently use headings and bullet/numbered lists, but SwiftUI's
// `AttributedString(markdown:)` only interprets *inline* syntax (bold/italic/links/code) —
// block elements are dropped. This parses the text into blocks and renders each with its own
// styling, still using AttributedString for the inline emphasis within a block. Self-contained
// (no external Markdown dependency). Code fences / block quotes / rules aren't styled yet —
// they fall through to plain paragraphs.

import SwiftUI

struct MarkdownBlocksView: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            ForEach(Array(MarkdownBlock.parse(text).enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: MarkdownBlock) -> some View {
        switch block {
        case let .heading(level, text):
            inline(text)
                .font(headingFont(level))
                .fontWeight(.semibold)
                .padding(.top, level <= 2 ? 4 : 1)
                .frame(maxWidth: .infinity, alignment: .leading)
        case let .bullet(text):
            listRow(marker: Text("•"), text: text)
        case let .numbered(marker, text):
            listRow(marker: Text("\(marker).").monospacedDigit(), text: text)
        case let .paragraph(text):
            inline(text)
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func listRow(marker: Text, text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            marker
                .font(.callout)
                .foregroundStyle(Color.panelSecondary)
            inline(text)
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.leading, 4)
    }

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: return .title3
        case 2: return .headline
        default: return .subheadline
        }
    }

    /// A single line's inline Markdown (bold / italic / `code` / links) as a `Text`, falling
    /// back to the literal string if it doesn't parse.
    private func inline(_ s: String) -> Text {
        if let a = try? AttributedString(
            markdown: s, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        {
            return Text(a)
        }
        return Text(s)
    }
}

/// One parsed Markdown block. Deliberately small — the shapes AI descriptions actually use.
enum MarkdownBlock {
    case heading(level: Int, text: String)
    case bullet(text: String)
    case numbered(marker: String, text: String)
    case paragraph(text: String)

    static func parse(_ md: String) -> [MarkdownBlock] {
        var blocks: [MarkdownBlock] = []
        var para: [String] = []
        func flushParagraph() {
            if !para.isEmpty {
                blocks.append(.paragraph(text: para.joined(separator: " ")))
                para.removeAll()
            }
        }

        for raw in md.components(separatedBy: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty {
                flushParagraph()
                continue
            }

            // ATX heading: 1–6 '#' followed by a space.
            let hashes = line.prefix(while: { $0 == "#" })
            if !hashes.isEmpty, hashes.count <= 6, line.dropFirst(hashes.count).first == " " {
                flushParagraph()
                blocks.append(
                    .heading(
                        level: hashes.count,
                        text: String(line.dropFirst(hashes.count)).trimmingCharacters(
                            in: .whitespaces)))
                continue
            }

            // Bullet list: '-', '*' or '+' then a space.
            if let first = line.first, "-*+".contains(first), line.dropFirst().first == " " {
                flushParagraph()
                blocks.append(.bullet(text: String(line.dropFirst(2))))
                continue
            }

            // Numbered list: digits, '.', space (and not a decimal like "3.5").
            if let dot = line.firstIndex(of: "."), dot != line.startIndex,
                line[line.startIndex..<dot].allSatisfy(\.isNumber),
                line.index(after: dot) < line.endIndex, line[line.index(after: dot)] == " "
            {
                flushParagraph()
                blocks.append(
                    .numbered(
                        marker: String(line[line.startIndex..<dot]),
                        text: String(line[line.index(after: dot)...]).trimmingCharacters(
                            in: .whitespaces)))
                continue
            }

            para.append(line)
        }
        flushParagraph()
        return blocks
    }
}
