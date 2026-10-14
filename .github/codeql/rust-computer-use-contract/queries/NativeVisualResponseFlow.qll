import rust
import codeql.rust.controlflow.BasicBlocks
import codeql.rust.controlflow.CfgNodes
import codeql.rust.controlflow.ControlFlowGraph
import codeql.rust.dataflow.DataFlow
import codeql.rust.dataflow.TaintTracking

/**
 * Shared, deliberately bounded response-flow facts for native visual
 * computer-use handlers.
 *
 * The Rust data-flow library is used only for local value-preserving moves,
 * aliases, and ordinary reborrows.  Wrapper summaries are not inferred from a
 * function name: a guard is trusted only when its body still downgrades a
 * response whose image proof is absent.  Unknown interprocedural and
 * collection edges remain unknown and are intentionally not treated as proof.
 */

predicate visualProviderFile(File file) {
  file.getRelativePath() = "codex-rs/android-computer-use/src/lib.rs" or
  file.getRelativePath() = "codex-rs/browser-computer-use/src/lib.rs" or
  // Keep the historical fixture path for the existing focused query test.
  file.getRelativePath() = "codex-rs/computer-use-runtime/src/lib.rs"
}

predicate productionVisualProviderFile(File file) {
  file.getRelativePath() = "codex-rs/android-computer-use/src/lib.rs" or
  file.getRelativePath() = "codex-rs/browser-computer-use/src/lib.rs"
}

predicate visualHandler(Function function) {
  visualProviderFile(function.getFile()) and
  (
    function.getName().getText() = "observe" or
    function.getName().getText() = "step" or
    function.getName().getText() = "install_build_from_run" or
    function.getName().getText() = "handle_with_provider"
  )
}

predicate productionVisualHandler(Function function) {
  productionVisualProviderFile(function.getFile()) and
  (
    function.getName().getText() = "observe" or
    function.getName().getText() = "step" or
    function.getName().getText() = "install_build_from_run" or
    function.getName().getText() = "handle_with_provider"
  )
}

predicate variableAccessExpr(Expr expr, Variable variable) {
  exists(VariableAccess access |
    expr = access and
    access.getVariable() = variable
  )
}

/** A variable access through the ordinary `&mut response` reborrow shape. */
predicate responseVariableExpr(Expr expr, Variable variable) {
  variableAccessExpr(expr, variable) or
  exists(RefExpr ref |
    expr = ref and
    variableAccessExpr(ref.getExpr(), variable)
  )
}

/** A local variable initialized from a reference to another response. */
predicate referenceAlias(Variable alias, Variable target) {
  exists(RefExpr reference |
    alias.getInitializer() = reference and
    responseVariableExpr(reference.getExpr(), target)
  )
}

/** A local alias initialized from the response's content-items field. */
predicate contentItemsAlias(Variable alias, Variable responseVariable) {
  exists(RefExpr reference, FieldExpr field |
    alias.getInitializer() = reference and
    reference.getExpr() = field and
    field.hasContainer() and
    field.hasIdentifier() and
    field.getIdentifier().getText() = "content_items" and
    responseVariableExpr(field.getContainer(), responseVariable)
  )
}

/**
 * Local value flow covers aliases and ordinary borrow/reborrow edges without
 * claiming a universal interprocedural or collection model.
 */
predicate localValueFlow(Expr sourceExpr, Expr sinkExpr) {
  exists(DataFlow::ExprNode source, DataFlow::ExprNode sink |
    source.asExpr() = sourceExpr and
    sink.asExpr() = sinkExpr and
    DataFlow::localFlow(source, sink)
  )
}

predicate visualGuardCall(Call call, Function handler) {
  call.getEnclosingCallable() = handler and
  exists(Function target |
    target = call.getStaticTarget() and
    target.getName().getText() = "require_native_image_for_visual_response" and
    visualProviderFile(target.getFile())
  )
}

predicate visualGuardTarget(Call call, Function target) {
  exists(Function resolved |
    resolved = call.getStaticTarget() and
    target = resolved and
    target.getName().getText() = "require_native_image_for_visual_response" and
    visualProviderFile(target.getFile())
  )
}

predicate successfulResultExit(Function function, Expr exitExpr, Expr responseExpr) {
  exitExpr.getEnclosingCallable() = function and
  exists(TupleVariantExpr ok |
    ok = exitExpr and
    ok.getVariant().getName().getText() = "Ok" and
    responseExpr = ok.getArgList().getArg(0)
  ) and
  (
    exists(ReturnExpr returnExpr | returnExpr.getExpr() = exitExpr) or
    function.getBody().getStmtList().getTailExpr() = exitExpr
  )
}

predicate guardUsesReturnedResponse(Call guard, Expr responseExpr) {
  exists(Variable variable |
    responseVariableExpr(responseExpr, variable) and
    responseVariableExpr(guard.getPositionalArgument(0), variable)
  ) or
  exists(Variable responseVariable, Variable guardVariable |
    responseVariableExpr(responseExpr, responseVariable) and
    responseVariableExpr(guard.getPositionalArgument(0), guardVariable) and
    (
      referenceAlias(guardVariable, responseVariable) or
      referenceAlias(responseVariable, guardVariable)
    )
  ) or
  localValueFlow(responseExpr, guard.getPositionalArgument(0)) or
  localValueFlow(guard.getPositionalArgument(0), responseExpr)
}

predicate cfgNodeForExpr(Expr expr, CfgNode node) {
  node = expr.getACfgNode()
}

predicate blockNodeOrder(BasicBlock block, CfgNode earlier, CfgNode later) {
  exists(int earlierIndex, int laterIndex |
    block.getNode(earlierIndex) = earlier and
    block.getNode(laterIndex) = later and
    earlierIndex <= laterIndex
  )
}

predicate guardDominatesExit(Call guard, Expr exitExpr) {
  exists(CfgNode guardNode, CfgNode exitNode, BasicBlock guardBlock, BasicBlock exitBlock |
    exists(CallCfgNode callNode |
      callNode.getCall() = guard and
      guardNode = callNode
    ) and
    cfgNodeForExpr(exitExpr, exitNode) and
    guardBlock.getANode() = guardNode and
    exitBlock.getANode() = exitNode and
    (
      guardBlock.strictlyDominates(exitBlock) or
      guardBlock = exitBlock and blockNodeOrder(guardBlock, guardNode, exitNode)
    )
  )
}

/** Calls whose contract already returns `success: false`; they are safe. */
predicate definitelyFailedResponse(Expr responseExpr) {
  exists(Call call |
    (
      call = responseExpr or
      exists(AwaitExpr await |
        await = responseExpr and
        call = await.getExpr()
      )
    ) and
    exists(Function target |
      target = call.getStaticTarget() and
      (
        target.getName().getText() = "action_failure_response" or
        target.getName().getText() = "failed_response"
      )
    )
  )
}

/**
 * A write after the guard invalidates the response-version proof.  The
 * assignment AST catches direct local replacement; the narrow field spelling
 * catches image removal and success restoration while leaving metadata-only
 * edits alone.
 */
predicate nodeBefore(CfgNode earlier, CfgNode later) {
  exists(BasicBlock earlierBlock, BasicBlock laterBlock |
    earlierBlock.getANode() = earlier and
    laterBlock.getANode() = later and
    (
      earlierBlock.strictlyDominates(laterBlock) or
      earlierBlock = laterBlock and blockNodeOrder(earlierBlock, earlier, later)
    )
  )
}

predicate cfgNodeBetween(Call guard, Expr mutation, Expr exitExpr) {
  exists(CallCfgNode guardNode, CfgNode writeNode, CfgNode exitNode |
    guardNode.getCall() = guard and
    cfgNodeForExpr(mutation, writeNode) and
    cfgNodeForExpr(exitExpr, exitNode) and
    nodeBefore(guardNode, writeNode) and
    nodeBefore(writeNode, exitNode)
  )
}

predicate responseFieldWrite(AssignmentExpr assignment, Variable variable) {
  exists(FieldExpr field |
    assignment.getLhs() = field and
    field.hasContainer() and
    field.hasIdentifier() and
    responseVariableExpr(field.getContainer(), variable) and
    (
      field.getIdentifier().getText() = "content_items" or
      field.getIdentifier().getText() = "success" or
      field.getIdentifier().getText() = "error"
    )
  )
}

/** A destructive clear of the response's native-image content collection. */
predicate responseContentItemsClear(MethodCallExpr clearCall, Variable variable) {
  clearCall.getIdentifier().getText() = "clear" and
  exists(FieldExpr field |
    field.hasContainer() and
    field.hasIdentifier() and
    field.getIdentifier().getText() = "content_items" and
    responseVariableExpr(field.getContainer(), variable) and
    (
      clearCall.getReceiver() = field or
      localValueFlow(field, clearCall.getReceiver()) or
      exists(VariableAccess access |
        clearCall.getReceiver() = access and
        contentItemsAlias(access.getVariable(), variable)
      )
    )
  )
}

predicate responseWriteAfterGuard(
  Function function,
  Call guard,
  Expr exitExpr,
  Expr responseExpr
) {
  exists(Variable variable, AssignmentExpr assignment |
    responseVariableExpr(responseExpr, variable) and
    assignment.getEnclosingCallable() = function and
    cfgNodeBetween(guard, assignment, exitExpr) and
    (
      exists(VariableWriteAccess write |
        write = assignment.getAWriteAccess() and
        write.getVariable() = variable
      ) or
      responseFieldWrite(assignment, variable)
    )
  )
}

predicate responseContentItemsClearAfterGuard(
  Function function,
  Call guard,
  Expr exitExpr,
  Expr responseExpr
) {
  exists(Variable variable, MethodCallExpr clearCall |
    responseVariableExpr(responseExpr, variable) and
    clearCall.getEnclosingCallable() = function and
    cfgNodeBetween(guard, clearCall, exitExpr) and
    responseContentItemsClear(clearCall, variable)
  )
}

/**
 * Keep helper recognition body-backed.  The guard must still force a failed
 * response when image output is absent; naming a function alone is not enough.
 */
/** A bounded, local write of the supplied diagnostic into the supplied content. */
predicate helperItemsReceiver(Function helper, Expr receiver) {
  exists(Variable items |
    items.getParameter() = helper.getParam(0) and responseVariableExpr(receiver, items)
  )
}

/** The mutable text binding extracted directly from the supplied first item. */
predicate helperTextReceiver(Function helper, Expr receiver) {
  exists(LetExpr binding, MethodCallExpr first, StructPatField field, Variable text |
    binding.getEnclosingCallable() = helper and
    binding.getScrutinee() = first and
    first.getIdentifier().getText() = "first_mut" and
    helperItemsReceiver(helper, first.getReceiver()) and
    field.getParentNode*() = binding.getPat() and
    field.getFieldName() = "text" and
    field.getPat() = text.getPat() and
    responseVariableExpr(receiver, text)
  )
}

predicate helperDetailMutation(Function helper, MethodCallExpr mutation) {
  exists(
    DataFlow::ParameterNode message, DataFlow::ExprNode text
  |
    message.isParameterOf(helper, 1) and
    mutation.getEnclosingCallable() = helper and
    (
      mutation.getIdentifier().getText() = "push_str" and
      helperTextReceiver(helper, mutation.getReceiver()) and
      text.asExpr() = mutation.getPositionalArgument(0)
      or
      helperItemsReceiver(helper, mutation.getReceiver()) and
      exists(Expr item |
        (
          mutation.getIdentifier().getText() = "insert" and
          item = mutation.getPositionalArgument(1)
          or
          mutation.getIdentifier().getText() = "push" and
          item = mutation.getPositionalArgument(0)
        ) and
        (
          text.asExpr() = item or
          text.asExpr() = item.(StructExpr).getFieldExpr("text").getExpr()
        )
      )
    ) and
    TaintTracking::localTaint(message, text)
  )
}

/** Reachability that deliberately stops at a message-bearing content write. */
predicate detailFreeReachable(Function helper, CfgNode node) {
  node.getEnclosingCallable() = helper and
  not exists(MethodCallExpr mutation |
    helperDetailMutation(helper, mutation) and node = mutation.getACfgNode()
  ) and
  (
    not exists(node.getAPredecessor()) or
    detailFreeReachable(helper, node.getAPredecessor())
  )
}

/**
 * Trust only the small current append helper vocabulary. Unknown calls and
 * assignments fail closed, including clear, replacement, detached clones and
 * arbitrary mutable borrows. No interprocedural mutation summary is inferred.
 */
predicate helperOperationKnown(Function helper, Call call) {
  exists(MethodCallExpr method |
    call = method and
    (
      helperDetailMutation(helper, method) or
      method.getIdentifier().getText() = "first_mut" and
      helperItemsReceiver(helper, method.getReceiver()) or
      exists(DataFlow::ParameterNode message, DataFlow::ExprNode receiver |
        message.isParameterOf(helper, 1) and
        receiver.asExpr() = method.getReceiver() and
        TaintTracking::localTaint(message, receiver) and
        method.getIdentifier().getText() = ["trim", "to_string"]
      )
    )
  )
}

predicate helperRetainsFailureDetail(Function helper) {
  exists(MethodCallExpr mutation | helperDetailMutation(helper, mutation)) and
  not exists(AssignmentExpr assignment | assignment.getEnclosingCallable() = helper) and
  not exists(Call call |
    call.getEnclosingCallable() = helper and not helperOperationKnown(helper, call)
  ) and
  not exists(CfgNode exit |
    exit.getEnclosingCallable() = helper and
    not exists(exit.getASuccessor()) and
    detailFreeReachable(helper, exit)
  )
}

predicate guardWritesFailureDetail(
  Function target, AssignmentExpr successWrite, FieldExpr successField
) {
  exists(AssignmentExpr errorWrite, FieldExpr errorField |
    errorWrite.getEnclosingCallable() = target and
    errorWrite.getLhs() = errorField and
    errorField.hasIdentifier() and
    errorField.getIdentifier().getText() = "error"
  ) or
  exists(
    Call diagnostic, Function helper, RefExpr borrow, FieldExpr contentItems,
    Variable responseVariable
  |
    diagnostic.getEnclosingCallable() = target and
    helper = diagnostic.getStaticTarget() and
    helper.getName().getText() = "append_text" and
    helper.getFile() = target.getFile() and
    diagnostic.getPositionalArgument(0) = borrow and
    borrow.getExpr() = contentItems and
    contentItems.hasContainer() and
    contentItems.hasIdentifier() and
    contentItems.getIdentifier().getText() = "content_items" and
    responseVariableExpr(contentItems.getContainer(), responseVariable) and
    successField.hasContainer() and
    responseVariableExpr(successField.getContainer(), responseVariable) and
    // Every helper completion must retain the actual diagnostic in the actual
    // content, and the call must precede the failed-success write.
    helperRetainsFailureDetail(helper) and
    nodeBefore(diagnostic.getACfgNode(), successWrite.getACfgNode())
  )
}

predicate guardBodyDowngrades(Call guard) {
  exists(Function target |
    visualGuardTarget(guard, target) and
    (
      // The legacy fixture intentionally models only the call-site contract;
      // production paths must satisfy the body-backed helper check below.
      not productionVisualProviderFile(target.getFile()) or
      (
        exists(AssignmentExpr successWrite, FieldExpr successField, BooleanLiteralExpr falseValue |
          successWrite.getEnclosingCallable() = target and
          successWrite.getLhs() = successField and
          successField.hasIdentifier() and
          successField.getIdentifier().getText() = "success" and
          falseValue = successWrite.getRhs() and
          falseValue.getTextValue() = "false" and
          guardWritesFailureDetail(target, successWrite, successField)
        )
      )
    )
  )
}

predicate responseVersionGuarded(
  Function function,
  Expr exitExpr,
  Expr responseExpr
) {
  exists(Call guard |
    visualGuardCall(guard, function) and
    guardUsesReturnedResponse(guard, responseExpr) and
    guardDominatesExit(guard, exitExpr) and
    not responseWriteAfterGuard(function, guard, exitExpr, responseExpr) and
    not responseContentItemsClearAfterGuard(function, guard, exitExpr, responseExpr) and
    guardBodyDowngrades(guard)
  )
}
