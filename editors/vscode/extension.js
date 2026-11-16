'use strict';

const vscode = require('vscode');

function activate(context) {
  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory('zkcl', {
    createDebugAdapterDescriptor() {
      const executable = vscode.workspace.getConfiguration('zkcl').get('executablePath', 'zkcl');
      return new vscode.DebugAdapterExecutable(executable, ['dap']);
    }
  }));
  context.subscriptions.push(vscode.debug.registerDebugConfigurationProvider('zkcl', {
    resolveDebugConfiguration(_folder, config) {
      if (!config.type && !config.request && !config.name) {
        config.type = 'zkcl';
        config.request = 'launch';
        config.name = 'Debug Lisp file';
        config.program = '${file}';
        config.cwd = '${workspaceFolder}';
      }
      return config;
    }
  }));
}

module.exports = { activate };
