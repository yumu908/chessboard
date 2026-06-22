import fs from 'fs';
import path from 'path';
import JavaScriptObfuscator from 'javascript-obfuscator';

const distDir = path.resolve('dist');

function walkDir(dir, callback) {
    if (!fs.existsSync(dir)) {
        return;
    }
    fs.readdirSync(dir).forEach(f => {
        let dirPath = path.join(dir, f);
        let isDirectory = fs.statSync(dirPath).isDirectory();
        if (isDirectory) {
            walkDir(dirPath, callback);
        } else {
            callback(dirPath);
        }
    });
}

console.log('开始混淆 dist 目录下的 JS 文件...');

let count = 0;
walkDir(distDir, (filePath) => {
    if (filePath.endsWith('.js')) {
        console.log(`正在混淆: ${filePath}`);
        const code = fs.readFileSync(filePath, 'utf8');
        try {
            const obfuscationResult = JavaScriptObfuscator.obfuscate(code, {
                compact: true,
                controlFlowFlattening: true,
                controlFlowFlatteningThreshold: 0.5,
                numbersToExpressions: true,
                simplify: true,
                stringArray: true,
                stringArrayRotate: true,
                stringArrayShuffle: true,
                stringArrayThreshold: 0.75,
                transformObjectKeys: false,
                unicodeEscapeSequence: true,
                renameGlobals: false // 关键选项：避免重命名全局变量破坏 Tauri 窗口通信 API
            });
            fs.writeFileSync(filePath, obfuscationResult.getObfuscatedCode());
            count++;
        } catch (err) {
            console.error(`混淆失败: ${filePath}`, err);
        }
    }
});

console.log(`JS 混淆完成！共处理 ${count} 个文件。`);
