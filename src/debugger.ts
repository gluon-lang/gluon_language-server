/*
Original code borrowed from https://github.com/vadimcn/vscode-lldb but tweaked to
work with gluon.

The MIT License (MIT)

Copyright (c) 2016 Vadim Chugunov

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

*/

import * as cp from 'child_process';
import * as path from 'path';

function send_error_msg(slideout: string, message: string) {
    // Send this info as a console event
    let event = JSON.stringify({
        type: 'event', seq: 0, event: 'output', body:
        { category: 'console', output: message }
    });
    process.stdout.write('\r\nContent-Length: ' + event.length.toString() + '\r\n\r\n');
    process.stdout.write(event);
    // Also, fake out a response to 'initialize' message, which will be shown on a slide-out.
    let response = JSON.stringify({
        type: 'response', command: 'initialize', request_seq: 1, success: false, body:
            { error: { id: 0, format: slideout, showUser: true } }
    });
    process.stdout.write('Content-Length: ' + response.length.toString() + '\r\n\r\n');
    process.stdout.write(response);
}

let gluon_debugger = cp.spawn('gluon_debugger', [], {
    stdio: ['pipe', 'pipe'],
    cwd: __dirname + '/..'
});

// In case there are problems with launching...
gluon_debugger.on('error', (err: any) => {
    send_error_msg('Failed to launch gluon_debugger: ' + err.message, err.message);
    process.exit(1);
});
 
gluon_debugger.stdout.setEncoding('utf8');

// When gluon_debugger exits, we exit too
gluon_debugger.on('exit', (code: number) => {
    process.exit(code);
});

process.stdin.pipe(gluon_debugger.stdin);
gluon_debugger.stdout.pipe(process.stdout);
