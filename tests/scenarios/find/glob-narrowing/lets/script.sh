mkdir web && printf 'const needle = 1;\n' > web/app.ts && printf 'let needle = 2;\n' > web/app.js && printf 'needle: 3\n' > web/config.yml

lets find needle web -g '*.ts'

lets find needle web
