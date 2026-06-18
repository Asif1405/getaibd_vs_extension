import * as vscode from 'vscode';
import { AgentViewProvider } from './AgentViewProvider';

/**
 * Called when the extension is activated.
 * Activation is triggered when the webview view becomes visible.
 */
export function activate(context: vscode.ExtensionContext) {
  console.log('GetAIBD extension is now active.');

  // 1. Register the Agent panel webview view provider
  const agentProvider = new AgentViewProvider(context.extensionUri);

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(
      AgentViewProvider.viewType,
      agentProvider,
      { webviewOptions: { retainContextWhenHidden: true } }
    )
  );

  // 2. Watch for API key changes → auto-refresh models
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('getaibd.apiKey')) {
        const apiKey = vscode.workspace
          .getConfiguration('getaibd')
          .get<string>('apiKey', '');
        if (apiKey) {
          agentProvider.refreshModels();
        }
      }
    })
  );

  // 3. Register the "Refresh Models" command
  context.subscriptions.push(
    vscode.commands.registerCommand('getaibd.refreshModels', () => {
      agentProvider.refreshModels();
    })
  );

  // 4. Register the "Open Settings" command
  context.subscriptions.push(
    vscode.commands.registerCommand('getaibd.openSettings', () => {
      vscode.commands.executeCommand(
        'workbench.action.openSettings',
        'getaibd'
      );
    })
  );
}

/**
 * Called when the extension is deactivated.
 */
export function deactivate() {}
