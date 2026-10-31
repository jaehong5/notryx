// @vitest-environment jsdom
import { expect, test } from "vitest";
import { prepareMermaid } from "./mermaid-engine";

test.each([
  'flowchart LR\nA[한글]-->B', 'sequenceDiagram\nA->>B: 한글',
  'classDiagram\nA --> B', 'stateDiagram-v2\nA --> B', 'erDiagram\nA ||--o{ B : has',
])("parses supported diagrams under the fixed policy: %s", async (source) => {
  await expect(prepareMermaid(source, "light")).resolves.toBeUndefined();
});

test.each([
  'flowchart LR\n' + Array.from({ length: 501 }, (_, i) => `A${i}-->A${i + 1}`).join('\n'),
  'sequenceDiagram\n' + 'A->>B: message\n'.repeat(501),
  'classDiagram\n' + Array.from({ length: 501 }, (_, i) => `A${i}-->A${i + 1}`).join('\n'),
  'stateDiagram-v2\n' + Array.from({ length: 501 }, (_, i) => `A${i}-->A${i + 1}`).join('\n'),
  'erDiagram\n' + Array.from({ length: 501 }, (_, i) => `A${i} ||--o{ A${i + 1} : has`).join('\n'),
])("rejects more than 500 parsed connections before layout", async (source) => {
  await expect(prepareMermaid(source, "light")).rejects.toThrow(/mermaid_edge_limit|Edge limit exceeded/u);
});

test("keeps unverified diagram types and document config as source", async () => {
  await expect(prepareMermaid('pie\n"A": 1', "light")).rejects.toThrow("mermaid_unsupported_diagram");
  await expect(prepareMermaid('%%{init:{"htmlLabels":"false"}}%%\nflowchart LR\nA-->B', "light")).rejects.toThrow("mermaid_document_config");
});
