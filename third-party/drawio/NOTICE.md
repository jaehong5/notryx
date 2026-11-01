# draw.io 오프라인 편집기 자산

Notryx는 [draw.io v32.6.9](https://github.com/jgraph/drawio/releases/tag/v32.6.9)의 정적 자산 일부를 Linux 편집 창에 포함한다. 원본 commit은 `3e707fd20b6930778d6f6bf6e4d0079b4bce366b`이며 배포 archive의 SHA-256은 `manifest.json`에 고정한다. 편집기 실행에는 외부 서비스 계정이나 네트워크가 필요하지 않다. 최초 개발 빌드에는 archive 다운로드가 필요하며 이후 `--offline`으로 준비할 수 있다.

원본 코드의 라이선스는 `LICENSE.txt`의 Apache-2.0이다. Copyright (c) JGraph Holdings Ltd and draw.io AG. Notryx가 변경한 부분은 `index.html`의 bridge script 삽입, `js/PreConfig.js`, `js/PostConfig.js`, `js/notryx-bridge.js`다. upstream minified JavaScript는 변경하지 않는다.

`img`, `shapes`, `stencils`에는 별도의 사용 제한이 있다. 이 자산을 Atlassian 제품 또는 Atlassian marketplace/plugin 제품에 사용하는 경우 upstream의 서면 허락이 필요하다. 일반 사용자가 만든 다이어그램 출력에는 그 제한이 적용되지 않는다. 정확한 원문은 `licenses/img-LICENSE.txt`, `licenses/shapes-LICENSE.txt`, `licenses/stencils-LICENSE.txt`에 포함한다. 이 번들 전체를 Apache-2.0 단일 라이선스라고 표시하면 안 된다.

`js/app.min.js`에 포함된 라이브러리와 번들 폰트의 고지를 `licenses/`에 보관한다. 버전은 upstream의 [dependency manifest](https://github.com/jgraph/drawio/blob/3e707fd20b6930778d6f6bf6e4d0079b4bce366b/etc/dependencies/package.json)와 [build manifest](https://github.com/jgraph/drawio/blob/3e707fd20b6930778d6f6bf6e4d0079b4bce366b/etc/build/build.xml)를 기준으로 확인했다.

| 구성 요소 | 버전 | 라이선스 원문 |
| --- | --- | --- |
| mxGraph / draw.io | 위 고정 commit | `LICENSE.txt` |
| spin.js | 2.0.0 | `licenses/spin.txt` |
| DOMPurify | 3.4.17 | `licenses/DOMPurify.txt`, Apache-2.0 선택 |
| CryptoJS | 4.2.0 | `licenses/crypto-js.txt` |
| pako | 2.2.0 | `licenses/pako.txt`, `licenses/pako-zlib.txt` |
| roughjs | 4.6.6 | `licenses/rough.txt` |
| perfect-freehand | 1.2.3 | `licenses/perfect-freehand.txt` |
| Architects Daughter | archive의 TTF | `licenses/ArchitectsDaughter.txt` |

Android 빌드는 실제 편집기 자산을 제외한다. 일반 이미지 미리보기는 별도 기능이다. Java servlet, 외부 계정 연결, service worker, 임의 plugin, 원격 export, 별도 Mermaid/MathJax/PlantUML 변환기는 이 번들에 포함하지 않는다.
