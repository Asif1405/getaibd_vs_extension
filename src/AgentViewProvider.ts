import * as vscode from 'vscode';
import { fetchModels, generateResponse, Model } from './apiClient';
import { getWorkspaceContext, writeFileInWorkspace, deleteFileInWorkspace } from './workspaceUtil';

export class AgentViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'getaibd.agentView';
  private _view?: vscode.WebviewView;
  private _models: Model[] = [];

  constructor(private readonly _extensionUri: vscode.Uri) {}

  public resolveWebviewView(
    webviewView: vscode.WebviewView,
    _context: vscode.WebviewViewResolveContext,
    _token: vscode.CancellationToken
  ): void {
    this._view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [this._extensionUri],
    };
    webviewView.webview.html = this._getHtml();

    webviewView.webview.onDidReceiveMessage(async (message) => {
      switch (message.type) {
        case 'send': await this._handleSend(message.prompt); break;
        case 'selectModel': await this._handleModelSelect(message.modelId); break;
        case 'refreshModels': await this.refreshModels(); break;
        case 'ready': await this._sendInitialState(); break;
      }
    });
  }

  public async refreshModels(): Promise<void> {
    const config = vscode.workspace.getConfiguration('getaibd');
    const apiKey = config.get<string>('apiKey', '');
    if (!apiKey) {
      this._post({ type: 'error', message: 'Please set your API key in Settings (search "getaibd").' });
      return;
    }
    this._post({ type: 'loadingModels', loading: true });
    try {
      this._models = await fetchModels(apiKey);
      const selectedModel = config.get<string>('model', '');
      this._post({ type: 'modelsLoaded', models: this._models, selectedModel });
      if (this._models.length > 0) {
        vscode.window.showInformationMessage(`GetAIBD: ${this._models.length} model(s) loaded.`);
      } else {
        vscode.window.showWarningMessage('GetAIBD: No models returned.');
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Unknown error';
      this._post({ type: 'error', message: `Failed to fetch models: ${msg}` });
      vscode.window.showErrorMessage(`GetAIBD: ${msg}`);
    } finally {
      this._post({ type: 'loadingModels', loading: false });
    }
  }

  private async _handleSend(prompt: string): Promise<void> {
    if (!prompt.trim()) { return; }
    const config = vscode.workspace.getConfiguration('getaibd');
    const apiKey = config.get<string>('apiKey', '');
    const model = config.get<string>('model', '');
    if (!apiKey) {
      this._post({ type: 'error', message: 'API key not configured. Go to Settings → search "getaibd".' });
      return;
    }
    if (!model) {
      this._post({ type: 'error', message: 'No model selected. Please select a model from the dropdown.' });
      return;
    }

    this._post({ type: 'userMessage', content: prompt });
    this._post({ type: 'typing', isTyping: true });

    try {
      // 1. Gather workspace context
      const workspaceContext = await getWorkspaceContext();

      // 2. Build instructions & context prompt
      const systemPrompt = `You are a software engineer agent working in the user's workspace.
You have read their codebase, which is provided below.
Your task is to write code or modify files according to the user's instructions.

If you need to write or edit a file, output your code exactly in this block format:
<<<WRITE:path/to/filename.ext>>>
Write the COMPLETE file contents here. Do not omit code or use placeholder comments.
<<<END>>>

If you need to delete a file:
<<<DELETE:path/to/filename.ext>>>

Do not put anything inside these blocks other than the exact contents/paths. You may write multiple blocks if you need to create/update multiple files. Always explain what changes you made outside the blocks.

Here is the current codebase context:
${workspaceContext}
`;

      const finalPrompt = `${systemPrompt}\n\nUser Instruction: ${prompt}`;

      // 3. Request generation
      const response = await generateResponse(apiKey, model, finalPrompt);
      this._post({ type: 'typing', isTyping: false });

      // 4. Parse writes and deletes
      const writtenFiles: string[] = [];
      const deletedFiles: string[] = [];

      const writeRegex = /<<<WRITE:([^\n>]+)>>>\r?\n([\s\S]*?)\r?\n<<<END>>>/g;
      let writeMatch;
      while ((writeMatch = writeRegex.exec(response)) !== null) {
        const filePath = writeMatch[1].trim();
        const content = writeMatch[2];
        try {
          await writeFileInWorkspace(filePath, content);
          writtenFiles.push(filePath);
        } catch (writeErr) {
          const msg = writeErr instanceof Error ? writeErr.message : 'Unknown write error';
          this._post({ type: 'error', message: `Failed to write file ${filePath}: ${msg}` });
        }
      }

      const deleteRegex = /<<<DELETE:([^\n>]+)>>>/g;
      let deleteMatch;
      while ((deleteMatch = deleteRegex.exec(response)) !== null) {
        const filePath = deleteMatch[1].trim();
        try {
          await deleteFileInWorkspace(filePath);
          deletedFiles.push(filePath);
        } catch (delErr) {
          const msg = delErr instanceof Error ? delErr.message : 'Unknown delete error';
          this._post({ type: 'error', message: `Failed to delete file ${filePath}: ${msg}` });
        }
      }

      // Remove the raw protocol blocks from the display text response
      let cleanResponse = response
        .replace(/<<<WRITE:([^\n>]+)>>>\r?\n[\s\S]*?\r?\n<<<END>>>/g, '')
        .replace(/<<<DELETE:([^\n>]+)>>>/g, '')
        .trim();

      if (writtenFiles.length > 0 || deletedFiles.length > 0) {
        let summary = '\n\n**Local operations executed:**';
        if (writtenFiles.length > 0) {
          summary += writtenFiles.map(f => `\n- 📝 Written/Updated: \`${f}\``).join('');
        }
        if (deletedFiles.length > 0) {
          summary += deletedFiles.map(f => `\n- 🗑️ Deleted: \`${f}\``).join('');
        }
        cleanResponse += summary;
      }

      this._post({ type: 'aiMessage', content: cleanResponse || 'All file operations executed successfully.' });
    } catch (err) {
      this._post({ type: 'typing', isTyping: false });
      const msg = err instanceof Error ? err.message : 'Unknown error';
      this._post({ type: 'error', message: `Execution failed: ${msg}` });
    }
  }

  private async _handleModelSelect(modelId: string): Promise<void> {
    const config = vscode.workspace.getConfiguration('getaibd');
    await config.update('model', modelId, vscode.ConfigurationTarget.Global);
    vscode.window.showInformationMessage(`GetAIBD: Model set to "${modelId}".`);
  }

  private async _sendInitialState(): Promise<void> {
    const config = vscode.workspace.getConfiguration('getaibd');
    const apiKey = config.get<string>('apiKey', '');
    const selectedModel = config.get<string>('model', '');
    if (apiKey && this._models.length === 0) {
      await this.refreshModels();
    } else {
      this._post({ type: 'modelsLoaded', models: this._models, selectedModel });
    }
    if (!apiKey) {
      this._post({ type: 'info', message: 'Welcome to GetAIBD! Set your API key in Settings to get started.' });
    }
  }

  private _post(message: unknown): void {
    if (this._view) { this._view.webview.postMessage(message); }
  }

  private _getHtml(): string {
    const nonce = getNonce();
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1.0"/>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'nonce-${nonce}'; script-src 'nonce-${nonce}';"/>
<title>GetAIBD Agent</title>
<style nonce="${nonce}">${CSS_CONTENT}</style>
</head>
<body>
<div class="container">
<div class="header">
<div class="hbrand"><div class="logo">G</div><span class="htitle">GetAIBD</span></div>
<div class="model-wrap">
<select id="modelSelect" class="model-select" title="Select AI Model"><option value="">— Select Model —</option></select>
<button id="refreshBtn" class="refresh-btn" title="Refresh Models">
<svg width="14" height="14" viewBox="0 0 16 16" fill="none"><path d="M13.65 2.35A8 8 0 1 0 16 8h-2a6 6 0 1 1-1.76-4.24L10 6h6V0l-2.35 2.35z" fill="currentColor"/></svg>
</button>
</div>
</div>
<div id="messages" class="messages">
<div id="welcome" class="welcome"><div class="wicon">✦</div><h3>GetAIBD Agent</h3><p>Your AI-powered coding assistant. Select a model above and type your prompt below.</p></div>
</div>
<div id="typingIndicator" class="typing-ind"><div class="dot"></div><div class="dot"></div><div class="dot"></div></div>
<div class="input-area">
<div class="input-wrap"><textarea id="promptInput" class="prompt-input" placeholder="Type your prompt here..." rows="1"></textarea></div>
<button id="sendBtn" class="send-btn" title="Send"><svg viewBox="0 0 24 24" fill="none"><path d="M2.01 21L23 12 2.01 3 2 10l15 2-15 2z" fill="currentColor"/></svg></button>
</div>
</div>
<script nonce="${nonce}">${JS_CONTENT}</script>
</body></html>`;
  }
}

function getNonce(): string {
  let text = '';
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  for (let i = 0; i < 32; i++) { text += chars.charAt(Math.floor(Math.random() * chars.length)); }
  return text;
}

const CSS_CONTENT = `
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
html,body{height:100%;overflow:hidden;font-family:var(--vscode-font-family,system-ui,sans-serif);font-size:var(--vscode-font-size,13px);color:var(--vscode-foreground);background:var(--vscode-panel-background,var(--vscode-editor-background))}
.container{display:flex;flex-direction:column;height:100vh}
.header{display:flex;align-items:center;gap:8px;padding:10px 14px;border-bottom:1px solid var(--vscode-panel-border,rgba(255,255,255,.08));background:var(--vscode-sideBar-background,transparent);flex-shrink:0}
.hbrand{display:flex;align-items:center;gap:6px;flex-shrink:0}
.logo{width:20px;height:20px;border-radius:4px;background:linear-gradient(135deg,#667eea,#764ba2);display:flex;align-items:center;justify-content:center;font-weight:bold;font-size:11px;color:#fff}
.htitle{font-weight:600;font-size:12px;opacity:.9;letter-spacing:.3px}
.model-wrap{flex:1;display:flex;align-items:center;gap:6px;justify-content:flex-end}
.model-select{flex:1;max-width:200px;padding:4px 22px 4px 8px;border-radius:4px;border:1px solid var(--vscode-dropdown-border,rgba(255,255,255,.12));background:var(--vscode-dropdown-background,var(--vscode-input-background));color:var(--vscode-dropdown-foreground,var(--vscode-foreground));font-size:11px;font-family:inherit;outline:none;cursor:pointer;appearance:none;-webkit-appearance:none;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath fill='%23888' d='M0 0l5 6 5-6z'/%3E%3C/svg%3E");background-repeat:no-repeat;background-position:right 6px center}
.model-select:focus{border-color:var(--vscode-focusBorder)}
.refresh-btn{display:flex;align-items:center;justify-content:center;width:24px;height:24px;border:none;border-radius:4px;background:transparent;color:var(--vscode-foreground);cursor:pointer;opacity:.7;transition:opacity .15s,background .15s}
.refresh-btn:hover{opacity:1;background:var(--vscode-toolbar-hoverBackground,rgba(255,255,255,.1))}
.refresh-btn.spinning svg{animation:spin .8s linear infinite}
@keyframes spin{from{transform:rotate(0)}to{transform:rotate(360deg)}}
.messages{flex:1;overflow-y:auto;padding:12px 14px;display:flex;flex-direction:column;gap:10px;scroll-behavior:smooth}
.messages::-webkit-scrollbar{width:6px}
.messages::-webkit-scrollbar-thumb{background:var(--vscode-scrollbarSlider-background,rgba(255,255,255,.15));border-radius:3px}
.message{display:flex;flex-direction:column;max-width:92%;animation:fadeIn .25s ease-out}
@keyframes fadeIn{from{opacity:0;transform:translateY(6px)}to{opacity:1;transform:translateY(0)}}
.message.user{align-self:flex-end}
.message.ai{align-self:flex-start}
.message.system,.message.error{align-self:center;max-width:100%}
.message-label{font-size:10px;font-weight:600;text-transform:uppercase;letter-spacing:.5px;margin-bottom:4px;opacity:.55}
.message.user .message-label{text-align:right;color:#667eea}
.message.ai .message-label{color:#43b581}
.message-body{padding:10px 14px;border-radius:12px;line-height:1.55;font-size:12.5px;white-space:pre-wrap;word-wrap:break-word}
.message.user .message-body{background:linear-gradient(135deg,rgba(102,126,234,.18),rgba(118,75,162,.18));border:1px solid rgba(102,126,234,.2);border-bottom-right-radius:4px}
.message.ai .message-body{background:var(--vscode-textCodeBlock-background,rgba(255,255,255,.05));border:1px solid var(--vscode-widget-border,rgba(255,255,255,.08));border-bottom-left-radius:4px}
.message.system .message-body{background:rgba(250,166,26,.08);border:1px solid rgba(250,166,26,.15);border-radius:8px;text-align:center;font-size:11.5px;opacity:.85}
.message.error .message-body{background:rgba(244,67,54,.08);border:1px solid rgba(244,67,54,.2);border-radius:8px;color:var(--vscode-errorForeground,#f44336);font-size:11.5px}
.typing-ind{display:none;align-self:flex-start;padding:10px 16px;margin:0 14px;background:var(--vscode-textCodeBlock-background,rgba(255,255,255,.05));border:1px solid var(--vscode-widget-border,rgba(255,255,255,.08));border-radius:12px;border-bottom-left-radius:4px}
.typing-ind.visible{display:flex;gap:4px;align-items:center}
.dot{width:6px;height:6px;border-radius:50%;background:var(--vscode-foreground);opacity:.4;animation:typing 1.2s ease-in-out infinite}
.dot:nth-child(2){animation-delay:.15s}
.dot:nth-child(3){animation-delay:.3s}
@keyframes typing{0%,60%,100%{opacity:.25;transform:translateY(0)}30%{opacity:.8;transform:translateY(-4px)}}
.welcome{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:12px;padding:20px;text-align:center}
.wicon{width:48px;height:48px;border-radius:14px;background:linear-gradient(135deg,#667eea,#764ba2);display:flex;align-items:center;justify-content:center;font-size:22px;color:#fff;box-shadow:0 4px 20px rgba(102,126,234,.25)}
.welcome h3{font-size:15px;font-weight:600;margin-top:4px}
.welcome p{font-size:12px;opacity:.65;line-height:1.5;max-width:260px}
.input-area{display:flex;align-items:flex-end;gap:8px;padding:10px 14px 12px;border-top:1px solid var(--vscode-panel-border,rgba(255,255,255,.08));background:var(--vscode-sideBar-background,transparent);flex-shrink:0}
.input-wrap{flex:1}
.prompt-input{width:100%;min-height:36px;max-height:120px;padding:8px 12px;border-radius:8px;border:1px solid var(--vscode-input-border,rgba(255,255,255,.12));background:var(--vscode-input-background);color:var(--vscode-input-foreground,var(--vscode-foreground));font-family:inherit;font-size:12.5px;line-height:1.5;resize:none;outline:none;transition:border-color .15s}
.prompt-input::placeholder{color:var(--vscode-input-placeholderForeground,rgba(255,255,255,.35))}
.prompt-input:focus{border-color:var(--vscode-focusBorder)}
.send-btn{display:flex;align-items:center;justify-content:center;width:36px;height:36px;border:none;border-radius:8px;background:linear-gradient(135deg,#667eea,#764ba2);color:#fff;cursor:pointer;font-size:16px;flex-shrink:0;transition:opacity .15s,transform .1s;box-shadow:0 2px 8px rgba(102,126,234,.3)}
.send-btn:hover{opacity:.9}
.send-btn:active{transform:scale(.95)}
.send-btn:disabled{opacity:.4;cursor:not-allowed;transform:none}
.send-btn svg{width:16px;height:16px}
`;

const JS_CONTENT = `
(function(){
var vscode=acquireVsCodeApi();
var messagesEl=document.getElementById('messages');
var welcomeEl=document.getElementById('welcome');
var promptInput=document.getElementById('promptInput');
var sendBtn=document.getElementById('sendBtn');
var modelSelect=document.getElementById('modelSelect');
var refreshBtn=document.getElementById('refreshBtn');
var typingIndicator=document.getElementById('typingIndicator');
var hasMessages=false;

promptInput.addEventListener('input',function(){
  promptInput.style.height='auto';
  promptInput.style.height=Math.min(promptInput.scrollHeight,120)+'px';
});

function sendMessage(){
  var prompt=promptInput.value.trim();
  if(!prompt)return;
  vscode.postMessage({type:'send',prompt:prompt});
  promptInput.value='';
  promptInput.style.height='auto';
  sendBtn.disabled=true;
}

sendBtn.addEventListener('click',sendMessage);
promptInput.addEventListener('keydown',function(e){
  if(e.key==='Enter'&&!e.shiftKey){e.preventDefault();sendMessage();}
});

modelSelect.addEventListener('change',function(){
  var v=modelSelect.value;
  if(v)vscode.postMessage({type:'selectModel',modelId:v});
});

refreshBtn.addEventListener('click',function(){
  vscode.postMessage({type:'refreshModels'});
});

function hideWelcome(){if(welcomeEl&&!hasMessages){welcomeEl.style.display='none';hasMessages=true;}}
function scrollBottom(){requestAnimationFrame(function(){messagesEl.scrollTop=messagesEl.scrollHeight;});}

function addMsg(type,content,label){
  hideWelcome();
  var msg=document.createElement('div');msg.className='message '+type;
  var lbl=document.createElement('div');lbl.className='message-label';lbl.textContent=label;
  var body=document.createElement('div');body.className='message-body';body.textContent=content;
  msg.appendChild(lbl);msg.appendChild(body);messagesEl.appendChild(msg);scrollBottom();
}

window.addEventListener('message',function(event){
  var msg=event.data;
  switch(msg.type){
    case 'userMessage':addMsg('user',msg.content,'You');break;
    case 'aiMessage':addMsg('ai',msg.content,'AI');sendBtn.disabled=false;break;
    case 'error':addMsg('error',msg.message,'Error');sendBtn.disabled=false;break;
    case 'info':addMsg('system',msg.message,'Info');break;
    case 'modelsLoaded':
      modelSelect.innerHTML='<option value="">— Select Model —</option>';
      if(msg.models&&msg.models.length>0){
        msg.models.forEach(function(m){
          var opt=document.createElement('option');opt.value=m.id;opt.textContent=m.name;
          if(m.id===msg.selectedModel)opt.selected=true;
          modelSelect.appendChild(opt);
        });
      }
      break;
    case 'loadingModels':
      if(msg.loading)refreshBtn.classList.add('spinning');
      else refreshBtn.classList.remove('spinning');
      break;
    case 'typing':
      if(msg.isTyping){typingIndicator.classList.add('visible');scrollBottom();}
      else typingIndicator.classList.remove('visible');
      break;
  }
});

vscode.postMessage({type:'ready'});
})();
`;
