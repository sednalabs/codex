/**
 * @name Android visual computer-use tool missing native-image guard
 * @description Android observe/step handlers must require native image output before a visual response can be treated as successful.
 * @kind problem
 * @problem.severity error
 * @precision high
 * @id rust/android-visual-tool-missing-native-image-guard
 * @tags correctness
 *       maintainability
 *       computer-use
 */

import rust
import codeql.rust.controlflow.BasicBlocks
import codeql.rust.controlflow.CfgNodes
import codeql.rust.controlflow.ControlFlowGraph

predicate androidComputerUseProviderFile(File file) {
  file.getRelativePath() = "codex-rs/tui/src/android_computer_use_provider.rs"
}

predicate androidVisualToolHandler(Function function) {
  androidComputerUseProviderFile(function.getFile()) and
  (
    function.getName().getText() = "observe" or
    function.getName().getText() = "step" or
    function.getName().getText() = "install_build_from_run"
  )
}

predicate isNativeImageGuardCall(Call call) {
  exists(Function target |
    target = call.getStaticTarget() and
    target.getName().getText() = "require_native_image_for_visual_response" and
    androidComputerUseProviderFile(target.getFile())
  )
}

predicate isSuccessfulResultExitExpr(Function function, Expr exitExpr) {
  exitExpr.getEnclosingCallable() = function and
  exists(TupleVariantExpr ok |
    ok = exitExpr and
    ok.getVariant().getName().getText() = "Ok"
  ) and
  (
    exists(ReturnExpr returnExpr | returnExpr.getExpr() = exitExpr)
    or
    function.getBody().getStmtList().getTailExpr() = exitExpr
  )
}

predicate isGuardNode(Function function, CfgNode node) {
  exists(CallCfgNode callNode |
    node = callNode and
    callNode.getCall().getEnclosingCallable() = function and
    isNativeImageGuardCall(callNode.getCall())
  )
}

predicate variableAccessExpr(Expr expr, Variable variable) {
  exists(VariableAccess access |
    expr = access and
    access.getVariable() = variable
  )
}

predicate responseVariableExpr(Expr expr, Variable variable) {
  variableAccessExpr(expr, variable)
  or
  exists(RefExpr ref |
    expr = ref and
    variableAccessExpr(ref.getExpr(), variable)
  )
}

predicate guardCallUsesResponseVariable(Call call, Variable variable) {
  isNativeImageGuardCall(call) and
  responseVariableExpr(call.getPositionalArgument(0), variable)
}

predicate isGuardNodeForResponseVariable(Function function, Variable variable, CfgNode node) {
  exists(CallCfgNode callNode |
    node = callNode and
    callNode.getCall().getEnclosingCallable() = function and
    guardCallUsesResponseVariable(callNode.getCall(), variable)
  )
}

predicate successfulExitReturnsResponseVariable(Expr exitExpr, Variable variable) {
  exists(TupleVariantExpr ok |
    ok = exitExpr and
    ok.getVariant().getName().getText() = "Ok" and
    responseVariableExpr(ok.getArgList().getArg(0), variable)
  )
}

predicate cfgNodeForExpr(Expr expr, CfgNode node) {
  node.getAstNode() = expr
}

predicate blockNodeOrder(BasicBlock block, CfgNode earlier, CfgNode later) {
  exists(int earlierIndex, int laterIndex |
    block.getNode(earlierIndex) = earlier and
    block.getNode(laterIndex) = later and
    earlierIndex <= laterIndex
  )
}

predicate guardDominatesSuccessfulExit(Function function, Expr exitExpr) {
  exists(CfgNode guardNode, CfgNode exitNode, BasicBlock guardBlock, BasicBlock exitBlock |
    (
      exists(Variable returnedVariable |
        successfulExitReturnsResponseVariable(exitExpr, returnedVariable) and
        isGuardNodeForResponseVariable(function, returnedVariable, guardNode)
      )
      or
      not exists(Variable returnedVariable |
        successfulExitReturnsResponseVariable(exitExpr, returnedVariable)
      ) and
      isGuardNode(function, guardNode)
    ) and
    cfgNodeForExpr(exitExpr, exitNode) and
    guardBlock.getANode() = guardNode and
    exitBlock.getANode() = exitNode and
    (
      guardBlock.strictlyDominates(exitBlock)
      or
      guardBlock = exitBlock and blockNodeOrder(guardBlock, guardNode, exitNode)
    )
  )
}

// This covers the provider's common local-variable `Ok(response)` shape. A future
// data-flow version can extend the same identity check through aliases and helper
// return values rather than only direct local-variable accesses.
from Function function, Expr exitExpr
where
  androidVisualToolHandler(function) and
  isSuccessfulResultExitExpr(function, exitExpr) and
  not guardDominatesSuccessfulExit(function, exitExpr)
select exitExpr,
  "This successful Android visual computer-use response can exit without first requiring native image output. Call require_native_image_for_visual_response on the response before returning Ok(...)."
