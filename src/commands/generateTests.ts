import * as vscode from "vscode";
import { chat } from "../client";

export async function generateTests(_context: vscode.ExtensionContext) {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {return;}

  const selection = editor.selection;
  const selectedText = editor.document.getText(selection);
  const language = editor.document.languageId;
  const fileName = editor.document.fileName;

  const codeToTest = selectedText || editor.document.getText();
  if (!codeToTest.trim()) {
    vscode.window.showWarningMessage("No code to generate tests for");
    return;
  }

  const config = vscode.workspace.getConfiguration("getaibd");
  const provider = config.get<string>("chat.defaultProvider", "getaibd");

  const model = await vscode.window.showInputBox({
    prompt: "Model for test generation",
    value: config.get<string>("chat.defaultModel", ""),
    placeHolder: "Leave empty for provider default",
  });
  if (model === undefined) {return;}

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Generating tests...",
      cancellable: true,
    },
    async (_progress, token) => {
      try {
        const systemPrompt = buildSystemPrompt(language);
        const userPrompt = buildUserPrompt(codeToTest, language, fileName, !!selectedText);

        const response = await chat(provider, model || "", [
          { role: "system", content: systemPrompt },
          { role: "user", content: userPrompt },
        ]);

        if (token.isCancellationRequested) {return;}

        const testCode = extractCodeBlock(response.content, language);
        const doc = await vscode.workspace.openTextDocument({
          content: testCode,
          language: testLanguage(language),
        });
        await vscode.window.showTextDocument(doc, { viewColumn: vscode.ViewColumn.Beside });
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : "Test generation failed";
        vscode.window.showErrorMessage(message);
      }
    },
  );
}

function buildSystemPrompt(language: string): string {
  const frameworks: Record<string, string> = {
    rust: "Use #[cfg(test)] module with #[test] functions. Use assert!, assert_eq!, assert_ne!.",
    typescript: "Use the project's test framework (Jest, Vitest, or Bun test). Use describe/it/expect.",
    javascript: "Use the project's test framework (Jest, Vitest, or Mocha). Use describe/it/expect.",
    python: "Use pytest with descriptive function names. Use assert statements.",
    go: "Use the testing package. Create Test* functions with *testing.T.",
    java: "Use JUnit 5. Use @Test annotation and assertions.",
    csharp: "Use xUnit or NUnit. Use [Fact] or [Test] attributes.",
    ruby: "Use RSpec with describe/it/expect syntax.",
    swift: "Use XCTest framework with XCTAssert*.",
    kotlin: "Use JUnit 5 or kotlin.test. Use @Test and assertions.",
  };

  const framework = frameworks[language] || "Use the appropriate testing framework for the language.";

  return [
    "You are a senior developer writing comprehensive unit tests.",
    framework,
    "Generate tests that cover:",
    "- Happy path / normal behavior",
    "- Edge cases (empty inputs, boundary values, null/undefined)",
    "- Error handling paths",
    "- Important state transitions",
    "",
    "Output ONLY the test code. No explanations, no markdown fences.",
    "Include necessary imports/use statements.",
    "Use descriptive test names that explain what is being tested.",
  ].join("\n");
}

function buildUserPrompt(
  code: string,
  language: string,
  fileName: string,
  isSelection: boolean,
): string {
  const scope = isSelection ? "the selected code" : "the file";
  return `Generate unit tests for ${scope} below.\n\nFile: ${fileName}\nLanguage: ${language}\n\n${code}`;
}

function extractCodeBlock(content: string, _language: string): string {
  const fenceMatch = content.match(/```[\w]*\n([\s\S]*?)```/);
  if (fenceMatch) {return fenceMatch[1].trim();}
  return content.trim();
}

function testLanguage(language: string): string {
  const mapping: Record<string, string> = {
    typescriptreact: "typescript",
    javascriptreact: "javascript",
  };
  return mapping[language] || language;
}
