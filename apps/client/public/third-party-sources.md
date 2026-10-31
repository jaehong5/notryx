# 번들 의존성의 라이선스와 소스

빌드에 포함된 의존성의 버전, 저작권 고지와 라이선스 전문은 같은 디렉터리의 `third-party-licenses.md`에 있다. Vite가 실제 번들 모듈을 기준으로 이 파일을 생성한다.

Mermaid 미리보기에 추가한 주요 의존성:

- Mermaid 12.2.0, MIT: <https://github.com/mermaid-js/mermaid>
- CSS Tree 3.2.1, MIT: <https://github.com/csstree/csstree>
- Mermaid의 간접 의존성 elkjs 0.9.3, EPL-2.0: 소스와 빌드 절차는 <https://github.com/kieler/elkjs/tree/0.9.3>에서 제공한다. 태그의 commit은 `a8304cf79fde75bc2ab1a89d28320f53f8637436`이다. Notryx는 elkjs 소스를 수정하지 않는다.

설치할 패키지의 정확한 버전과 무결성 값은 저장소의 `package-lock.json`에 고정한다. 의존성의 라이선스는 Notryx 자체의 프로젝트 라이선스와 별개다.
