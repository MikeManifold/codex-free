# 目标 Mac 的本机代码签名

先查看登录钥匙串“我的证书”。如果已有适用的代码签名证书，且展开后可见配对私钥，优先复用。名称不能证明身份相同，以公开指纹和实际签名结果为准。无需导出原 Mac 的私钥。

没有适用身份时，在目标机系统“钥匙串访问 → 证书助理 → 创建证书”建立本机身份：名称用不含个人信息的 `Local Development Code Signing`，身份类型选择自签名根，证书类型选择代码签名，存入登录钥匙串。根据系统界面选择适当有效期并记录到本机；到期前需要另行规划身份迁移。新证书可由用户自有 App 共用，各 App 固定不同 identifier。

打开配对**私钥**的“访问控制”，选择确认访问的模式，始终允许列表只保留 `/usr/bin/codesign`。移除临时创建助手及其他不必要程序，不选允许所有应用。保存时需要的密码在系统窗口输入，不发到聊天。不要通过全局分区列表或“全部始终信任”规避认证。完成后重新查看 ACL，不能因为保存按钮成功就假定设置已落实。

公开身份列表可用：

```sh
/usr/bin/security find-identity -p codesigning "$HOME/Library/Keychains/login.keychain-db"
```

选择对应证书的 SHA-1 指纹交给 bootstrap。它只读取公开证书，存储 DER、指纹及固定 DR；实际私钥调用交给系统 `codesign`，不使用临时创建助手。只有实际签名和严格验证通过才算身份可用。若系统拒绝访问，停止该次流程，检查 ACL、证书用途和实际系统提示；错误码不能证明密码输错，不循环重试。

本流程不承诺私钥属性“不可导出”，也不主动导出私钥。它提供本机持久身份，不是面向公众分发的 Developer ID / 公证。新身份首次仍可能触发系统权限确认，以后固定身份能减少更新导致的失配，不能承诺永不弹窗。

依据：[Apple 创建自签名证书](https://support.apple.com/guide/keychain-access/kyca8916/mac)、[Apple 代码签名流程](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html)。
